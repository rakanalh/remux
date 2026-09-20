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




# ---------------------------------------------------------------------------
# Alt+<dir> at a view's edge cell must reach the sidebar.
#
# The user's report was "with the right sidebar hidden and reshown, Alt+l does
# not navigate into the widgets". The hide/show is a red herring -- the real
# condition is that a VIEW is active. `handle_view_command`'s directional branch
# ran `probe.move_focus` and then returned `Ok(true)` unconditionally, so a key
# at the view's edge was swallowed before `intercept_focus` ever saw it.
#
# These run against the user's own sidebar shape -- edge = "right", size = 30,
# the `agents` and `files` plugins -- rather than `placeholder`, so the harness
# is not testing a fixture the report never involved.
# ---------------------------------------------------------------------------

RIGHT_W = 30
RIGHT_X0 = COLS - RIGHT_W  # 90

# A colour nothing else in the default theme uses, so "the active border colour
# appears in the sidebar's columns" cannot be satisfied by anything but focus.
ACTIVE_FG = "ff00aa"

CFG_RIGHT = f"""
[appearance.theme]
frame_active_fg = "#{ACTIVE_FG}"

[[sidebar]]
edge = "right"
size = {RIGHT_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1

  [[sidebar.panel]]
  plugin = "files"
  weight = 1
"""

# Same theme, no `[[sidebar]]` at all: `panel_rects` is empty, so every sidebar
# code path is unreachable and a directional key at the view's edge has nowhere
# to go but the swallow.
CFG_RIGHT_NO_SIDEBAR = f"""
[appearance.theme]
frame_active_fg = "#{ACTIVE_FG}"
"""

ALT_H, ALT_J, ALT_K, ALT_L = b"\x1bh", b"\x1bj", b"\x1bk", b"\x1bl"

# Assembled BY the shell, never present in the line that is typed: a shell
# echoes what it is given, so a marker visible in the command proves nothing.
MARKER_CMD = "printf 'RAN:%s\\n' {}\r"


def active_fg_cells(t, x0, x1):
    """Every (row, col) in [x0, x1) painted in the active border colour."""
    out = []
    for y in range(t.rows):
        for x in range(x0, min(x1, t.cols)):
            if str(t.screen.buffer[y][x].fg) == ACTIVE_FG:
                out.append((y, x))
    return out


def sidebar_has_keyboard(t):
    """Does the right sidebar carry the focused colour on its frame/header?

    `draw_sidebar_frame` and `nav::draw_header` both ask
    `compositor::border_fg(theme, focused)`, so this is the same answer the
    chrome itself is painting from.
    """
    return bool(active_fg_cells(t, RIGHT_X0, COLS))


def focused_panel_header(t):
    """The header row of the right sidebar's panel that paints as FOCUSED.

    `nav::draw_header` colours a panel's title with
    `compositor::border_fg(theme, focused)`, so a header carrying the active
    colour is the panel the chrome believes has the keyboard -- and it is what
    the user reads to find out where their keys are going. Narrower than
    `sidebar_has_keyboard`, which the sidebar's own frame also satisfies.
    """
    for y in range(t.rows):
        row = t.rows_text()[y]
        for x in range(RIGHT_X0, min(COLS, t.cols)):
            if str(t.screen.buffer[y][x].fg) != ACTIVE_FG:
                continue
            if t.screen.buffer[y][x].data.strip() in ("", *BOX):
                continue
            # The sidebar is framed in the border style, so its bar carries a
            # box glyph on each side of the header text.
            return row[RIGHT_X0:].strip("".join(BOX) + " ")
    return None


def focused_cell_span(t):
    """(first, last) column of the view's ACTIVE cell border, or None."""
    cols = [x for (_, x) in active_fg_cells(t, 0, RIGHT_X0)]
    return (min(cols), max(cols)) if cols else None


def sidebar_left_frame(t):
    """The screen column the right sidebar's own frame starts at.

    Row 0 carries two top-left corners with a right sidebar up: the view's at
    column 0 and the sidebar's at the seam, so the LAST one is the sidebar's.
    Reading it rather than assuming `RIGHT_X0` is what lets a resize be
    observed.
    """
    at = t.rows_text()[0].rfind("\u256d")
    return at if at > 0 else None


def stty_reading_right(t):
    """Run `stty size` in the focused cell and read the line it printed.

    The right-sidebar twin of `stty_reading`: rows are split on the cell
    borders, so the sidebar's own columns cannot contribute a token. The LAST
    such reading on screen, not the first -- an earlier one scrolls up and
    would report the size from before whatever the test just changed.
    """
    t.send("stty size\r", 1.8)
    found = None
    for row in t.rows_text():
        for tok in row.split("\u2502"):
            parts = tok.strip().split()
            if len(parts) == 2 and all(p.isdigit() for p in parts):
                found = tok.strip()
    return found


def pane_focus_leaks(tail):
    """Every `PaneFocus*` the SERVER was asked to run in this slice of the log."""
    import re

    return re.findall(r"msg=Command\((PaneFocus\w+)\)", tail)


def focus_left_cell(t):
    """Put view focus on the LEFT cell, whatever it was on.

    `Alt+h` at the leftmost cell has nowhere to go (the sidebar is on the RIGHT),
    so repeating it is a safe no-op rather than a guess about where compose left
    the focus.
    """
    t.send(ALT_H, 0.6)
    t.send(ALT_H, 0.6)


def test_alt_l_at_a_views_right_edge_focuses_the_right_sidebar():
    """The user's exact scenario. A view is live, focus is on the rightmost
    cell, `Alt+l` must hand the keyboard to the sidebar.

    Before the fix the directional branch of `handle_view_command` returned
    `Ok(true)` whatever `move_focus` answered, so the key never reached
    `intercept_focus` and the sidebar could not be entered from a view at all.
    """
    name = "test_alt_l_at_a_views_right_edge_focuses_the_right_sidebar"
    t = Tui("/tmp/rmx-sbv7", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []

    # Harness self-test FIRST, on the path that already worked: with no view up,
    # `Alt+l` from the rightmost pane enters the sidebar. If `sidebar_has_keyboard`
    # cannot report a focused sidebar here, every assertion below is vacuous.
    t.send(ALT_L, 1.0)
    if not sidebar_has_keyboard(t):
        print("ABORT: the focus observable never fires even without a view --")
        print("       the assertions below would pass on a broken client.")
        t.dump("observable blind")
        t.kill()
        sys.exit(1)
    t.send(ALT_H, 1.0)
    if sidebar_has_keyboard(t):
        fails.append("Alt+h did not leave the sidebar on the no-view path")

    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    # Normalise, then step to the RIGHT cell so the key under test is at the edge.
    focus_left_cell(t)
    left_span = focused_cell_span(t)
    t.send(ALT_L, 0.8)
    right_span = focused_cell_span(t)
    if left_span is None or right_span is None:
        fails.append(f"no focused cell border found ({left_span}, {right_span})")
    elif right_span == left_span:
        fails.append(
            f"Alt+l inside the view did not move focus to the second cell: "
            f"{left_span} -> {right_span}"
        )
    if sidebar_has_keyboard(t):
        fails.append(
            "Alt+l from the LEFT cell entered the sidebar -- the rect the edge "
            "test ran against is one cell too generous"
        )

    log_before = t.log("server")
    t.send(ALT_L, 1.2)

    if not sidebar_has_keyboard(t):
        t.dump("Alt+l at the view's right edge")
        fails.append("Alt+l at the view's right edge did not focus the sidebar")
    leaked = pane_focus_leaks(t.log("server")[len(log_before):])
    if leaked:
        fails.append(f"the key was forwarded to the masked server as {leaked}")

    # Where the keys went has to be VISIBLE, not merely true: a user who cannot
    # see the focused panel cannot tell a captured key from a dropped one.
    header = focused_panel_header(t)
    if header is None:
        t.dump("no panel header painted focused")
        fails.append("no panel header painted in the focused colour")

    # Pin the keyboard on the TOP panel: `files` (the bottom one) acts on Enter
    # by opening a split, and the marker line below ends in one.
    t.send(ALT_K, 0.6)
    if focused_panel_header(t) != "Agents":
        fails.append(
            f"Alt+k did not land on the Agents panel: "
            f"{focused_panel_header(t)!r}"
        )
    t.send(MARKER_CMD.format(7), 1.5)
    hits = [(i, r.index("RAN:7")) for i, r in enumerate(t.rows_text()) if "RAN:7" in r]
    if hits:
        t.dump("marker leaked to a cell")
        fails.append(
            f"the keystrokes reached a view cell's pane instead of the sidebar "
            f"at {hits}"
        )
    if sidebar_has_keyboard(t) is False:
        fails.append("the sidebar lost focus while being typed into")

    return finish(t, name, fails)


def test_alt_h_from_a_panel_returns_to_the_cell_the_view_left_from():
    """The other half of the same bug: leaving.

    With focus in a panel and a view live, `Alt+h` was captured by the view and
    moved a CELL instead of returning to it. The `Sidebar` branch must skip
    `move_focus` entirely -- and the cell it returns to is the one the user left
    from, not cell 0.
    """
    name = "test_alt_h_from_a_panel_returns_to_the_cell_the_view_left_from"
    t = Tui("/tmp/rmx-sbv8", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.send(ALT_L, 0.8)          # -> the right cell
    right_span = focused_cell_span(t)
    t.send(ALT_L, 1.2)          # -> the sidebar
    if not sidebar_has_keyboard(t):
        t.dump("never entered the sidebar")
        fails.append("Alt+l never focused the sidebar, so the leave path is untested")
        return finish(t, name, fails)

    log_before = t.log("server")
    t.send(ALT_H, 1.2)
    if sidebar_has_keyboard(t):
        t.dump("Alt+h from the panel")
        fails.append("Alt+h did not leave the sidebar")
    leaked = pane_focus_leaks(t.log("server")[len(log_before):])
    if leaked:
        fails.append(f"leaving the panel forwarded {leaked} to the masked server")

    back_span = focused_cell_span(t)
    if back_span != right_span:
        fails.append(
            f"focus came back to a different cell: left from {right_span}, "
            f"returned to {back_span}"
        )

    # And the keyboard really is the view's again -- in the RIGHT cell's pane.
    t.send(MARKER_CMD.format(8), 1.5)
    hits = [(i, r.index("RAN:8")) for i, r in enumerate(t.rows_text()) if "RAN:8" in r]
    if not hits:
        t.dump("marker never landed")
        fails.append("after leaving the panel a keystroke reached no pane at all")
    elif back_span is not None and any(x < back_span[0] for _, x in hits):
        fails.append(
            f"the keystroke landed left of the cell it should have returned to: "
            f"{hits} vs cell span {back_span}"
        )

    # Escape is the other way out, and in a view only `paint_view` puts the
    # cell's cursor back -- `chrome.paint` alone would leave it where the panel
    # had it.
    t.send(ALT_L, 1.2)
    if not sidebar_has_keyboard(t):
        fails.append("Alt+l did not re-enter the sidebar for the Escape case")
    t.send(b"\x1b", 1.2)
    if sidebar_has_keyboard(t):
        fails.append("Escape did not release the panel while a view was live")
    if t.screen.cursor.hidden:
        fails.append("Escape out of a panel left the view's cursor hidden")
    if "View 1" not in t.rows_text()[-1]:
        fails.append(f"Escape left the view: {t.rows_text()[-1].rstrip()!r}")

    return finish(t, name, fails)


def test_alt_l_from_a_non_edge_cell_still_moves_within_the_view():
    """A cell with a neighbour to its right keeps its key.

    This is what pins the rect convention: the edge test runs against the
    focused CELL's interior, so a non-edge cell can never satisfy it.
    """
    name = "test_alt_l_from_a_non_edge_cell_still_moves_within_the_view"
    t = Tui("/tmp/rmx-sbv9", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    before = focused_cell_span(t)
    log_before = t.log("server")
    t.send(ALT_L, 1.0)
    after = focused_cell_span(t)

    if sidebar_has_keyboard(t):
        t.dump("sidebar stole a non-edge key")
        fails.append("Alt+l from the left cell entered the sidebar")
    if before is None or after is None:
        fails.append(f"no focused cell border found ({before}, {after})")
    elif after == before:
        fails.append(f"Alt+l did not move view focus: {before} -> {after}")
    elif after[0] <= before[0]:
        fails.append(f"Alt+l moved focus LEFT: {before} -> {after}")
    leaked = pane_focus_leaks(t.log("server")[len(log_before):])
    if leaked:
        fails.append(f"a within-view move forwarded {leaked} to the masked server")

    return finish(t, name, fails)


def test_alt_l_at_a_views_edge_with_no_sidebar_reaches_no_server():
    """With no sidebar to enter the key stays swallowed.

    `handle_view_command` must not become `Ok(probe.move_focus(..))`: the
    foreground server is MASKED while a view is up, and a `PaneFocusRight`
    arriving there moves focus in a session the user is not looking at.
    """
    name = "test_alt_l_at_a_views_edge_with_no_sidebar_reaches_no_server"
    t = Tui("/tmp/rmx-sbv10", cols=COLS, rows=ROWS, config=CFG_RIGHT_NO_SIDEBAR).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.send(ALT_L, 0.8)          # -> the rightmost cell
    log_before = t.log("server")
    t.send(ALT_L, 1.2)          # at the edge, nothing to enter
    t.send(ALT_L, 1.2)

    leaked = pane_focus_leaks(t.log("server")[len(log_before):])
    if leaked:
        fails.append(f"a swallowed key leaked to the masked server as {leaked}")
    if "View 1" not in t.rows_text()[-1]:
        fails.append(f"no longer in the view: {t.rows_text()[-1].rstrip()!r}")

    return finish(t, name, fails)


def test_alt_l_with_only_the_focused_cell_placed_does_not_panic():
    """Zoom hides every cell but the focused one, so `cell_rects` is all `None`
    bar one. The rect lookup has to cope with that without indexing blindly.

    The zoomed cell fills the whole cell area, so it IS at the right edge and
    entering the sidebar is the CORRECT outcome -- the thing being pinned is
    that the client survives the layout at all.
    """
    name = "test_alt_l_with_only_the_focused_cell_placed_does_not_panic"
    t = Tui("/tmp/rmx-sbv11", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.prefix(b"f", 1.2)         # PaneToggleZoom -> ViewToggleZoom
    if " Z" not in t.rows_text()[-1]:
        fails.append(f"the view did not zoom: {t.rows_text()[-1].rstrip()!r}")

    log_before = t.log("server")
    t.send(ALT_L, 1.2)
    t.send(ALT_L, 1.2)
    if not t.alive():
        fails.append("the client died on a directional key in a zoomed view")
        return finish(t, name, fails)
    if not sidebar_has_keyboard(t):
        t.dump("zoomed cell at the edge")
        fails.append(
            "the zoomed cell spans the whole cell area, so Alt+l should have "
            "entered the sidebar"
        )
    leaked = pane_focus_leaks(t.log("server")[len(log_before):])
    if leaked:
        fails.append(f"a zoomed-view key leaked to the masked server as {leaked}")

    return finish(t, name, fails)




def test_hiding_the_sidebar_from_inside_it_does_not_strand_the_keyboard():
    """`Prefix b l` stays allowed while a view is live, so it can be pressed from
    INSIDE the sidebar it hides. The keyboard must come back to the view.

    `toggle_edge` drops focus to the content when it hides the sidebar holding
    it, but nothing proved that on this path -- and a panel that is no longer
    painted cannot show the user where their keys went.
    """
    name = "test_hiding_the_sidebar_from_inside_it_does_not_strand_the_keyboard"
    t = Tui("/tmp/rmx-sbv12", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.send(ALT_L, 0.8)          # -> the right cell
    right_span = focused_cell_span(t)
    t.send(ALT_L, 1.2)          # -> the sidebar
    if not sidebar_has_keyboard(t):
        t.dump("never entered the sidebar")
        fails.append("Alt+l never focused the sidebar, so the hide path is untested")
        return finish(t, name, fails)

    t.prefix(b"bl", 1.8)        # the default sidebar group, toggle right
    if t.has("Agents"):
        fails.append("the sidebar is still painted after the toggle")
    if not t.alive():
        fails.append("the client died hiding the sidebar it was focused in")
        return finish(t, name, fails)

    # The keyboard is the view's again -- and in the cell it left from.
    t.send(MARKER_CMD.format(12), 1.8)
    hits = [(i, r.index("RAN:12")) for i, r in enumerate(t.rows_text()) if "RAN:12" in r]
    if not hits:
        t.dump("keyboard stranded after the hide")
        fails.append("after hiding the focused sidebar no keystroke reached a pane")
    elif right_span is not None and any(x < right_span[0] for _, x in hits):
        fails.append(
            f"the keystroke landed outside the cell the user left from: "
            f"{hits} vs {right_span}"
        )
    if "View 1" not in t.rows_text()[-1]:
        fails.append(f"the hide left the view: {t.rows_text()[-1].rstrip()!r}")

    return finish(t, name, fails)


def test_leaving_the_view_from_a_panel_hands_the_keyboard_to_the_session():
    """`enter_view` drops panel focus; the reverse has to be safe too.

    `leave_active_view` calls `chrome.leave_sidebar()`, which used to be a
    tidy-up of a state nothing could reach. It is a real transition now: the
    user really can be in a panel when the view closes.
    """
    name = "test_leaving_the_view_from_a_panel_hands_the_keyboard_to_the_session"
    t = Tui("/tmp/rmx-sbv13", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.send(ALT_L, 0.8)
    t.send(ALT_L, 1.2)          # -> the sidebar
    if not sidebar_has_keyboard(t):
        t.dump("never entered the sidebar")
        fails.append("Alt+l never focused the sidebar, so the exit path is untested")
        return finish(t, name, fails)

    t.prefix(b"wq", 2.5)        # leave the view
    if t.has("View 1"):
        fails.append("`Prefix w q` did not leave the view")
    if not t.alive():
        fails.append("the client died leaving a view from a panel")
        return finish(t, name, fails)
    if sidebar_has_keyboard(t):
        fails.append("focus stayed in the panel after the view closed")

    # The session has the keyboard back.
    t.send(MARKER_CMD.format(13), 1.8)
    if not any("RAN:13" in r for r in t.rows_text()):
        t.dump("keyboard lost on view exit")
        fails.append("after leaving the view from a panel no keystroke reached a pane")

    return finish(t, name, fails)


def test_a_resize_from_a_panel_inside_a_view_moves_the_sidebar_and_the_cells():
    """With a panel focused, `Resize*` is the SIDEBAR's -- and the view's cells
    have to be re-demanded at the content rect it moved.

    A sidebar resize that repainted without re-subscribing would leave each cell
    showing a pane reflowed to a rect that is no longer on screen; `stty size`
    inside the cell is what sees that, and a border check would not.
    """
    name = "test_a_resize_from_a_panel_inside_a_view_moves_the_sidebar_and_the_cells"
    t = Tui("/tmp/rmx-sbv14", cols=COLS, rows=ROWS, config=CFG_RIGHT).start()
    fails = []
    make_two_panes(t)
    compose_view(t)
    require_in_view(t, fails)

    focus_left_cell(t)
    t.send(ALT_L, 0.8)
    before = stty_reading_right(t)

    t.send(ALT_L, 1.2)          # -> the sidebar
    if not sidebar_has_keyboard(t):
        t.dump("never entered the sidebar")
        fails.append("Alt+l never focused the sidebar, so the resize path is untested")
        return finish(t, name, fails)

    seam_before = sidebar_left_frame(t)
    # `Prefix p R` is the sticky resize group. A RIGHT sidebar GROWS towards the
    # left (`resize_focused` pairs `(Right, Left)` as growth), so `h` is the
    # press that moves it -- `l` would shrink it into its panels' minimums.
    t.prefix(b"pRh", 2.0)
    seam_after = sidebar_left_frame(t)
    t.send(b"\x1b", 0.6)       # leave the sticky group

    if seam_before is None or seam_after is None:
        fails.append(f"no cell border found ({seam_before}, {seam_after})")
    elif seam_after == seam_before:
        t.dump("the sidebar did not move")
        fails.append(
            f"the resize did not move the sidebar's edge: it stayed at {seam_before}"
        )

    # Back into the view and re-read the CELL's own idea of its size.
    t.send(ALT_H, 1.2)
    if sidebar_has_keyboard(t):
        fails.append("Alt+h did not leave the sidebar after the resize")
    after = stty_reading_right(t)
    if before is None or after is None:
        fails.append(f"no stty reading ({before!r}, {after!r})")
    elif after == before:
        fails.append(
            f"the cell was never re-demanded at the new content width: "
            f"still {after!r}"
        )
    else:
        print(f"  the cell went from {before!r} to {after!r} across the resize")

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
        test_alt_l_at_a_views_right_edge_focuses_the_right_sidebar,
        test_alt_h_from_a_panel_returns_to_the_cell_the_view_left_from,
        test_alt_l_from_a_non_edge_cell_still_moves_within_the_view,
        test_alt_l_at_a_views_edge_with_no_sidebar_reaches_no_server,
        test_alt_l_with_only_the_focused_cell_placed_does_not_panic,
        test_hiding_the_sidebar_from_inside_it_does_not_strand_the_keyboard,
        test_leaving_the_view_from_a_panel_hands_the_keyboard_to_the_session,
        test_a_resize_from_a_panel_inside_a_view_moves_the_sidebar_and_the_cells,
    ):
        ok = test() and ok
    print("ALL PASS" if ok else "FAILURES")
    sys.exit(0 if ok else 1)
