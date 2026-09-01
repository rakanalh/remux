#!/usr/bin/env python3
"""A View composites into the CONTENT rect, not over the sidebars.

Spec assertion 6. A View is a client-side virtual tab whose cells alias real
panes; before this it built a FULL-terminal buffer and blitted it with
`render_full`, painting straight over any panel.

The four things that had to move together, and the assertion that pins each:

  paint_view          -> `test_view_paints_inside_the_content_rect`
                         (every border and the view's own status bar start at
                         or right of the seam; the panel still renders left of
                         it)
  subscribe_view_cells -> `test_view_cells_are_sized_to_the_content_rect`
                         (reads the CONTENT of a cell, not its frame: `stty
                         size` INSIDE the cell reports the interior the server
                         reflowed the pane to, and a line one column too long
                         must WRAP inside the cell rather than have its tail
                         cropped away. Borders can land in exactly the right
                         columns while the pane behind them is sized wrong,
                         which the geometry assertion above cannot see.)
  the mouse hit tests -> `test_clicking_a_view_cell_uses_content_coordinates`
                         (a screen column that resolves to a DIFFERENT cell
                         untranslated)
  view entry/exit      -> `test_entering_a_view_releases_sidebar_focus`
                         (chrome focus must not stay stranded in a panel the
                         view's keyboard cannot reach)

plus `test_toggling_a_sidebar_inside_a_view_reflows_the_cells`, which is only
meaningful now that the panels are on screen next to a live view.

EVERY test here runs with a sidebar configured. With no `[[sidebar]]` the
content rect IS the terminal, `panel_rects` is empty and every translation is
the identity -- a sidebar-less run would be structurally blind to all of it.

Run from the repo root:  python3 tests/pty/sidebar_view.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui, sm_compose_view  # noqa: E402

COLS, ROWS = 120, 40
SIDEBAR_W = 30
CONTENT_W = COLS - SIDEBAR_W  # 90

# The default `grid` layout puts two cells side by side across the cell area,
# and a zellij-style border costs a column on each side of each cell:
#   content 90 -> two 45-wide cells -> 43 columns of pane inside each
#   whole terminal 120 -> two 60-wide cells -> 58   (what the bug produced)
# Rows: 40 - 1 status row - 2 border rows = 37.
CELL_STTY = "37 43"
BUGGY_STTY = "37 58"
NO_SIDEBAR_STTY = "37 58"

MARK_A = "AAAA_view_marker"
MARK_B = "BBBB_view_marker"

# 43 filler columns then a tail: in a correctly-sized cell the line is exactly
# one column too long and the tail wraps onto the next row. A cell whose PANE
# still thinks it is 58 columns wide does not wrap at all, and the blit crops
# the row at the cell's 43 painted columns -- so the tail is simply GONE. That
# missing tail is the user-visible symptom, and it is what this asserts.
WRAP_FILL = "W" * 43
WRAP_TAIL = "WRAPTAIL"

BOX = set("╭╮╰╯│─┌┐└┘├┤┬┴┼")

CFG = f"""
[keybindings.command]
"Alt-1" = "SidebarToggleLeft"
"Alt-2" = "SidebarFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

# The same sidebar plus a which-key leaf for a FOCUS intent. `SidebarFocus*`
# has no default binding, and reaching the refusal path below needs the intent
# to arrive from the which-key TREE (so a popup is up when it is refused), not
# from a flat Alt- shortcut.
CFG_FOCUS_LEAF = CFG + """
[keybindings.command.b]
H = "SidebarFocusLeft"
"""


# SGR mouse reports. Coordinates are 1-based, as a real terminal sends them.
def sgr_press(col, row):
    return f"\x1b[<0;{col};{row}M".encode()


def sgr_release(col, row):
    return f"\x1b[<0;{col};{row}m".encode()


def panel_marker(t):
    """The placeholder's focus marker as it currently renders, or None."""
    for row in t.rows_text():
        cell = row[:SIDEBAR_W]
        if "focused" in cell:
            return "focused"
        if "idle" in cell:
            return "idle"
    return None


def make_two_panes(t):
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.8)
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_B}\\n'\r", 0.6)


def compose_view(t):
    """Mark both panes in the session manager and compose them into a view."""
    sm_compose_view(t, panes=(0, 1), settle=2.0)
    t.pump(1.0)


def require_in_view(t, fails):
    """Hard gate: abort unless a VIEW is really on screen.

    Without it a failed compose leaves the NORMAL tab up, whose panes happen to
    sit inside the content rect already -- every geometry assertion below would
    pass while testing nothing.
    """
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    if "View 1" not in t.rows_text()[-1]:
        reasons.append(f"status bar is not a view bar: {t.rows_text()[-1].rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title on any border")
    if reasons:
        print("ABORT: never entered the view -- the assertions would be vacuous:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a view")
        t.kill()
        sys.exit(1)


def leftmost_box_column(t, start=0):
    """The leftmost column at or after `start` holding a box-drawing glyph.

    `start` exists because the sidebar is now framed in the same border style as
    the panes: its own box puts glyphs at column 0, so a caller asking "where do
    the VIEW's cells begin" has to skip the sidebar's columns explicitly.
    """
    best = None
    for row in t.rows_text():
        for x, ch in enumerate(row):
            if x < start:
                continue
            if ch in BOX:
                if best is None or x < best:
                    best = x
                break
    return best


def cell_columns(t, mark):
    """(start, end) screen columns of the cell showing `mark`, from its borders.

    Located from the marker's row: walk left and right to the nearest vertical
    border. Used to say which cell the hardware cursor is sitting in.
    """
    for row in t.rows_text():
        at = row.find(mark)
        if at < 0:
            continue
        left = at
        while left > 0 and row[left - 1] not in BOX:
            left -= 1
        right = at
        while right < len(row) - 1 and row[right + 1] not in BOX:
            right += 1
        return left, right
    return None


def stty_reading(t):
    """Run `stty size` in the focused cell and read the line it printed.

    The LAST such line on screen, not the first: a second reading in the same
    cell leaves the earlier one scrolled above it, and returning that would
    report the size from before whatever the test just changed.
    """
    t.send("stty size\r", 1.8)
    found = None
    for row in t.rows_text():
        body = row[SIDEBAR_W:] if row[:SIDEBAR_W].strip() in ("", "Placeholder") else row
        for tok in body.split("│"):
            tok = tok.strip()
            parts = tok.split()
            if len(parts) == 2 and all(p.isdigit() for p in parts):
                found = tok
    return found


def finish(t, name, fails):
    alive = t.alive()
    logs = (t.log("client") + t.log("server")).lower()
    if not alive:
        fails.append("the client died")
    if "panicked" in logs:
        fails.append("a panic in the logs")
    t.kill()
    if fails:
        print(f"FAIL {name}")
        for f in fails:
            print(f"  - {f}")
        return False
    print(f"PASS {name}")
    return True


def test_view_paints_inside_the_content_rect():
    """The view's own chrome starts at the seam; the panel keeps its columns."""
    name = "test_view_paints_inside_the_content_rect"
    t = Tui("/tmp/rmx-sbv1", cols=COLS, rows=ROWS, config=CFG).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    # The sidebar now has a box of its own on the two outer columns of its bar,
    # so "the leftmost box glyph on screen" is no longer the view's. What the
    # view must not do is paint a border in the sidebar's INTERIOR, which holds
    # only the placeholder's text -- and its own leftmost border must sit
    # exactly on the seam.
    all_rows = t.rows_text()
    intruders = [
        (y, x)
        for y, row in enumerate(all_rows[1 : len(all_rows) - 1], start=1)
        for x, ch in enumerate(row[1 : SIDEBAR_W - 1], start=1)
        if ch in BOX
    ]
    if intruders:
        fails.append(f"a view cell border painted inside the sidebar at {intruders[:4]}")
    box = leftmost_box_column(t, SIDEBAR_W)
    if box is None:
        fails.append("no cell border rendered at all")
    elif box != SIDEBAR_W:
        fails.append(
            f"the view's leftmost border is at column {box}, not on the seam "
            f"({SIDEBAR_W})"
        )

    bar = t.rows_text()[-1]
    if "View 1" not in bar:
        fails.append(f"the view status bar is missing: {bar.rstrip()!r}")
    elif bar.index("View 1") < SIDEBAR_W:
        fails.append(
            f"the view status bar starts at column {bar.index('View 1')}, "
            f"inside the sidebar"
        )
    # The sidebar's own bottom border owns that row now, so the band must be
    # exactly that border -- any other glyph there is the view's status bar
    # having run past the seam.
    if set(bar[:SIDEBAR_W]) - set("\u2570\u2500\u256f"):
        fails.append(
            f"the view status bar overwrote the panel's bottom row: "
            f"{bar[:SIDEBAR_W]!r}"
        )

    # And the panel is still there -- the whole point is that it survived.
    if not t.has("Placeholder"):
        fails.append("the panel stopped rendering once the view took the screen")
    if panel_marker(t) is None:
        fails.append("the panel's body row is gone")

    if not t.has(MARK_A) or not t.has(MARK_B):
        fails.append("a cell lost its pane content")
    for mark in (MARK_A, MARK_B):
        row = next(r for r in t.rows_text() if mark in r)
        if row.index(mark) < SIDEBAR_W:
            fails.append(f"{mark} painted at column {row.index(mark)}, in the sidebar")

    return finish(t, name, fails)


def test_view_cells_are_sized_to_the_content_rect():
    """A cell's PANE is reflowed to the cell's interior, not the terminal's.

    This is the half `test_view_paints_inside_the_content_rect` cannot see: the
    subscription width is what the server reflows the source pane to, so a stale
    full-terminal subscription puts correctly-placed borders around a pane that
    thinks it is 58 columns wide inside a 43-column cell.
    """
    name = "test_view_cells_are_sized_to_the_content_rect"
    t = Tui("/tmp/rmx-sbv2", cols=COLS, rows=ROWS, config=CFG).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    got = stty_reading(t)
    if got is None:
        t.dump("no stty reading")
        fails.append("`stty size` never printed inside the cell")
    elif got == BUGGY_STTY:
        fails.append(
            f"the cell's pane is sized to the WHOLE TERMINAL ({got}); the "
            f"subscription ignores the content rect"
        )
    elif got != CELL_STTY:
        fails.append(f"cell pane size {got!r}, expected {CELL_STTY!r}")
    else:
        print(f"  cell reports `stty size` = {got!r}")

    # The same fact read off the PAINTED CELL rather than out of the pane: the
    # size the server reports and the text a user actually sees are two
    # different failure surfaces, and only this one covers the blit.
    t.send(f"printf '{WRAP_FILL}{WRAP_TAIL}\\n'\r", 1.8)
    tail_rows = [i for i, r in enumerate(t.rows_text()) if WRAP_TAIL in r]
    # The command echo carries the tail too; the WRAPPED OUTPUT row is the one
    # that starts with the tail at the cell's left content edge.
    # ...i.e. it begins at the focused (left) cell's first content column:
    # the sidebar, then that cell's left border. Anchored on that KNOWN edge
    # rather than on "nothing but blanks to my left" -- the latter passes only
    # while the placeholder leaves its column mostly empty, and would start
    # rejecting real matches the moment a plugin that prints text on every row
    # (the Task 12 session tree) takes its place. The echoed command carries
    # the tail too, but never at the content edge.
    content_edge = SIDEBAR_W + 1
    wrapped = [
        i
        for i in tail_rows
        if t.rows_text()[i].index(WRAP_TAIL) == content_edge
    ]
    if not wrapped:
        t.dump("no wrapped tail")
        fails.append(
            f"{WRAP_TAIL!r} never wrapped onto its own row at the cell's "
            f"content edge (column {content_edge}): the pane did not wrap at "
            f"the cell's width and the blit cropped the tail away (tail seen "
            f"on rows {tail_rows}, at columns "
            f"{[t.rows_text()[i].index(WRAP_TAIL) for i in tail_rows]})"
        )
    else:
        print(f"  wrapped tail painted at column {content_edge}")

    return finish(t, name, fails)


def test_clicking_a_view_cell_uses_content_coordinates():
    """A click is translated out of screen coordinates before the hit test.

    Screen column 71 (1-based) is content column 40 -> the LEFT cell. Read
    untranslated it is column 70, which lands in the RIGHT cell whether the area
    is the terminal (cells 0..60, 60..120) or the content rect (0..45, 45..90).
    So this coordinate discriminates the fix from either way of getting it
    wrong.
    """
    name = "test_clicking_a_view_cell_uses_content_coordinates"
    t = Tui("/tmp/rmx-sbv3", cols=COLS, rows=ROWS, config=CFG).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    left = cell_columns(t, MARK_A)
    right = cell_columns(t, MARK_B)
    if left is None or right is None:
        t.dump("cells not locatable")
        fails.append("could not locate both cells on screen")
        return finish(t, name, fails)
    print(f"  left cell cols {left}, right cell cols {right}")
    if left[0] < SIDEBAR_W:
        fails.append(f"the left cell starts at {left[0]}, inside the sidebar")

    def cursor_in(span):
        return span[0] <= t.screen.cursor.x <= span[1]

    # Move focus to the RIGHT cell first, so the discriminating click below has
    # somewhere to move focus FROM. Done with the KEYBOARD (`Prefix l`), not a
    # click: no single screen column focuses the right cell both with and
    # without the translation, so a mouse setup would fail before the
    # discriminating click ever ran.
    t.send(b"\x1bl", 2.0)  # Alt-l = PaneFocusRight
    if not cursor_in(right):
        fails.append(
            f"Alt-l never focused the right cell: cursor at "
            f"x={t.screen.cursor.x}, right cell {right}"
        )
        t.dump("setup focus move")
        return finish(t, name, fails)

    # The discriminating click.
    t.send(sgr_press(71, 6), 0.6)
    t.send(sgr_release(71, 6), 2.0)
    if cursor_in(right):
        fails.append(
            f"a click at screen column 71 (content column 40) landed in the "
            f"RIGHT cell: the hit test ran on untranslated coordinates "
            f"(cursor x={t.screen.cursor.x})"
        )
    elif not cursor_in(left):
        fails.append(
            f"the click focused neither cell: cursor x={t.screen.cursor.x}, "
            f"left {left}, right {right}"
        )

    return finish(t, name, fails)


def test_entering_a_view_releases_sidebar_focus():
    """Step 0(a)/(b): a view takes the keyboard, so the chrome must let go.

    Reachable exactly as the task describes: the prefix passes through a focused
    panel, and the overlay it opens then owns the keyboard -- so `Prefix x m`
    composes a view with focus still parked in the panel.
    """
    name = "test_entering_a_view_releases_sidebar_focus"
    t = Tui("/tmp/rmx-sbv4", cols=COLS, rows=ROWS, config=CFG).start()
    fails = []
    make_two_panes(t)

    t.send(b"\x1b2", 0.8)  # Alt-2 = SidebarFocusLeft
    if panel_marker(t) != "focused":
        t.dump("sidebar not focused")
        fails.append("Alt-2 did not focus the panel; the rest would be vacuous")
        return finish(t, name, fails)

    compose_view(t)
    require_in_view(t, fails)

    marker = panel_marker(t)
    if marker is None:
        fails.append("the panel stopped rendering once the view took the screen")
    elif marker != "idle":
        fails.append(
            f"entering a view left chrome focus in the panel (marker={marker!r})"
            f" -- keys can never reach it while a view is up"
        )

    # And the keyboard really is the view's: type into the focused cell.
    t.send("printf 'ZZQQ_in_view\\n'\r", 1.5)
    if not t.has("ZZQQ_in_view"):
        t.dump("keystroke lost")
        fails.append("a keystroke in the view never reached a cell's pane")
    else:
        row = next(r for r in t.rows_text() if "ZZQQ_in_view" in r)
        if row.index("ZZQQ_in_view") < SIDEBAR_W:
            fails.append("the keystroke's echo landed inside the sidebar")

    # Leaving the view hands the screen back: the panel is painted and unfocused.
    t.prefix(b"wq", 2.0)
    if t.has("View 1"):
        fails.append("`Prefix w q` did not leave the view")
    if not t.has("Placeholder"):
        fails.append("the panel is gone after leaving the view")
    marker = panel_marker(t)
    if marker is None:
        fails.append("the panel's body row is gone after leaving the view")
    elif marker != "idle":
        fails.append(
            f"focus did not come back to the content on view exit "
            f"(marker={marker!r})"
        )

    return finish(t, name, fails)


def test_toggling_a_sidebar_inside_a_view_reflows_the_cells():
    """The panels are on screen next to a live view, so the toggle must work.

    Before this the intent was ignored while a view was up -- invisible then
    (the view covered the panel), a dropped keypress now.
    """
    name = "test_toggling_a_sidebar_inside_a_view_reflows_the_cells"
    t = Tui("/tmp/rmx-sbv5", cols=COLS, rows=ROWS, config=CFG).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    before = stty_reading(t)
    if before != CELL_STTY:
        fails.append(f"cell pane size before the toggle {before!r}, expected {CELL_STTY!r}")

    t.send(b"\x1b1", 1.5)  # Alt-1 = SidebarToggleLeft
    if t.has("Placeholder"):
        fails.append("the sidebar is still painted after the toggle")
    box = leftmost_box_column(t)
    if box != 0:
        fails.append(f"the view did not expand to column 0 after the toggle: {box}")

    got = stty_reading(t)
    if got != NO_SIDEBAR_STTY:
        fails.append(
            f"the cells were not re-demanded at the new content width: "
            f"{got!r}, expected {NO_SIDEBAR_STTY!r}"
        )
    else:
        print(f"  after hiding the sidebar the cell reports {got!r}")

    # ...and back.
    t.send(b"\x1b1", 1.5)
    if not t.has("Placeholder"):
        fails.append("the sidebar did not come back on the second toggle")
    if leftmost_box_column(t, SIDEBAR_W) != SIDEBAR_W:
        fails.append(
            f"the view did not shrink back to the seam: "
            f"{leftmost_box_column(t)}"
        )
    # The frame alone is not enough on this direction either -- that blindness
    # is the whole reason this task exists. Re-read the cell's own content.
    back = stty_reading(t)
    if back != CELL_STTY:
        fails.append(
            f"the cells were not re-demanded when the sidebar came back: "
            f"{back!r}, expected {CELL_STTY!r}"
        )
    else:
        print(f"  after showing the sidebar again the cell reports {back!r}")

    return finish(t, name, fails)


def test_a_focus_refused_by_a_live_view_does_not_strand_the_which_key_popup():
    """A view owns the keyboard, so a `SidebarFocus*` is refused -- and a
    refusal repaints NOTHING, which is how the popup used to be stranded.

    The `InputAction::Sidebar` arm had no `whichkey` teardown; it leaned on the
    `FullRender` that a real content-rect change provokes. This path changes no
    geometry at all, so that repaint never comes.
    """
    name = "test_a_focus_refused_by_a_live_view_does_not_strand_the_which_key_popup"
    t = Tui("/tmp/rmx-sbv6", cols=COLS, rows=ROWS, config=CFG_FOCUS_LEAF).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    seam_before = leftmost_box_column(t, SIDEBAR_W)

    t.send(b"\x01", 0.5)   # prefix
    t.send(b"b", 0.8)      # the Sidebar group
    if not t.has("cycle focus"):
        fails.append("the Sidebar which-key popup never opened")
    t.send(b"H", 1.2)      # SidebarFocusLeft -- refused while a view is live

    if t.has("cycle focus") or t.has("toggle left"):
        fails.append("the popup survived a focus intent refused by the live view")
        t.dump("popup stranded")
    if not t.alive():
        fails.append("the client died")
    # `clear_overlay` replays the front buffer with the cursor hidden, and in a
    # view only `paint_view` puts it back. Nothing else runs on this path.
    if t.screen.cursor.hidden:
        fails.append("the overlay teardown left the terminal cursor hidden")
    # The refusal must stay a refusal: the view keeps the screen and the panel
    # keeps its idle marker.
    if panel_marker(t) != "idle":
        fails.append(f"the panel took focus anyway: {panel_marker(t)!r}")
    if leftmost_box_column(t, SIDEBAR_W) != seam_before:
        fails.append(
            f"the view moved: seam {leftmost_box_column(t, SIDEBAR_W)} vs {seam_before}"
        )
    if "View 1" not in t.rows_text()[-1]:
        fails.append(f"no longer in the view: {t.rows_text()[-1].rstrip()!r}")

    return finish(t, name, fails)


if __name__ == "__main__":
    from pty_harness import BIN

    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    ok = True
    for test in (
        test_view_paints_inside_the_content_rect,
        test_view_cells_are_sized_to_the_content_rect,
        test_clicking_a_view_cell_uses_content_coordinates,
        test_entering_a_view_releases_sidebar_focus,
        test_toggling_a_sidebar_inside_a_view_reflows_the_cells,
        test_a_focus_refused_by_a_live_view_does_not_strand_the_which_key_popup,
    ):
        ok = test() and ok
    print("ALL PASS" if ok else "FAILURES")
    sys.exit(0 if ok else 1)
