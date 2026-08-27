#!/usr/bin/env python3
"""Sidebar layout: content offset, seam integrity, corner ownership.

Covers the design doc's test-table rows 1, 2, 3, 8 and 9 plus the visual-mode
crossing from section 7.

Run: python3 tests/pty/sidebar_layout.py
"""
import base64
import os
import re
import shutil
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath("target/debug/remux")
RUNDIR = "/tmp/rmx-sb"
COLS, ROWS = 100, 30
SIDEBAR_W = 30

SIDEBAR_CFG_LEFT = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

SIDEBAR_CFG_THREE = """
[[sidebar]]
edge = "left"
size = 20
  [[sidebar.panel]]
  plugin = "placeholder"

[[sidebar]]
edge = "right"
size = 16
  [[sidebar.panel]]
  plugin = "placeholder"

[[sidebar]]
edge = "bottom"
size = 5
  [[sidebar.panel]]
  plugin = "placeholder"
"""


def make_env(config: str) -> dict:
    shutil.rmtree(RUNDIR, ignore_errors=True)
    for sub in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR}/{sub}", exist_ok=True)
    os.makedirs(f"{RUNDIR}/config/remux", exist_ok=True)
    with open(f"{RUNDIR}/config/remux/config.toml", "w") as fh:
        fh.write(config)
    env = dict(os.environ)
    env.update(
        XDG_RUNTIME_DIR=f"{RUNDIR}/run",
        XDG_STATE_HOME=f"{RUNDIR}/state",
        XDG_DATA_HOME=f"{RUNDIR}/data",
        XDG_CONFIG_HOME=f"{RUNDIR}/config",
        SHELL="/bin/sh",
        ENV="/dev/null",
        TERM="xterm-256color",
        REMUX_ALLOW_NESTED="1",
        # A short, deterministic prompt: the visual-mode test asserts on the
        # column the block cursor lands in, which the inherited PS1 would move.
        PS1="> ",
    )
    return env


def spawn(env, cols=COLS, rows=ROWS):
    """Start the client under a PTY. Returns (child, screen, pump, raw).

    `raw` accumulates every byte the client emitted, so tests can decode the
    OSC 52 clipboard writes a real terminal would have acted on.
    """
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None)
    raw = bytearray()

    def pump(t=0.6):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            raw.extend(chunk)
            stream.feed(chunk)

    return child, screen, pump, raw


def yanks(raw):
    """Every clipboard write the client made, decoded, oldest first."""
    out = []
    for m in re.finditer(
        rb"\x1b\]52;[^;]*;([A-Za-z0-9+/=]*)(?:\x07|\x1b\\)", bytes(raw)
    ):
        try:
            out.append(base64.b64decode(m.group(1)).decode("utf-8", "replace"))
        except Exception:
            pass
    return out


def teardown(child, env):
    """Close the client and stop the throwaway server it auto-spawned."""
    try:
        child.close(force=True)
    except Exception:
        pass
    try:
        import subprocess

        subprocess.run(
            [BIN, "stop"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=10,
        )
    except Exception:
        pass


def check_no_panic():
    for name in ("client.log", "server.log"):
        log = f"{RUNDIR}/state/remux/{name}"
        if os.path.exists(log):
            body = open(log, errors="replace").read()
            assert "panicked" not in body, f"{name} panicked:\n{body[-2000:]}"


def test_no_sidebar_is_unchanged():
    env = make_env("")
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(b"echo NOSIDEBAR\r")
    pump(1.5)
    rows = screen.display
    hit = [r for r in rows if "NOSIDEBAR" in r]
    assert hit, "baseline (no sidebar) broke"
    # The pane border occupies the content origin, so shell output starts one
    # column in. What matters is that the frame itself begins at column 0.
    assert rows[0].startswith("\u256d"), f"pane frame not at column 0: {rows[0][:12]!r}"
    assert min(r.index("NOSIDEBAR") for r in hit) == 1, (
        "content should start at the terminal's left edge with no sidebar"
    )
    teardown(child, env)
    check_no_panic()
    print("PASS test_no_sidebar_is_unchanged")


def test_left_sidebar_offsets_content():
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)

    rows = screen.display
    # Assertion 1: the panel title occupies the sidebar.
    assert any(
        "Placeholder" in r[:SIDEBAR_W] for r in rows
    ), "panel not painted:\n" + "\n".join(rows[:5])

    # Write a marker into the shell and confirm it lands right of the seam.
    child.send(b"echo SEAMMARKER\r")
    pump(1.5)
    rows = screen.display
    hit = [i for i, r in enumerate(rows) if "SEAMMARKER" in r]
    assert hit, "marker never appeared"
    for i in hit:
        col = rows[i].index("SEAMMARKER")
        # Assertion 3: content lands at or right of the content origin.
        assert col >= SIDEBAR_W, f"content bled into the sidebar at col {col}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_left_sidebar_offsets_content")


def test_seam_has_no_background_bleed():
    """Assertion 2: the panel fills its rect and not one column more.

    Run with a distinct `frame_bg` so the panel's background is visible to
    pyte. The panel must own every column left of the seam, the pane's left
    border must sit exactly ON the seam, and the pane's interior must keep the
    terminal's own background -- if the panel's fill or its trailing SGR state
    ran past its rect, the interior would come back painted.

    (This replaces a `data != "█"` assertion carried in from the task
    brief, which could never fire: the placeholder fills with spaces, so no
    block glyph is ever emitted.)
    """
    env = make_env(SIDEBAR_CFG_LEFT_THEMED)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(b"echo SEAMMARKER\r")
    pump(1.5)
    rows = screen.display
    hit = [i for i, r in enumerate(rows) if "SEAMMARKER" in r]
    assert hit, "marker never appeared"
    y = hit[0]

    assert (
        str(screen.buffer[y][SIDEBAR_W - 1].bg) == FRAME_BG
    ), f"the panel does not own its last column: {screen.buffer[y][SIDEBAR_W - 1]!r}"
    assert (
        rows[y][SIDEBAR_W] == "│"
    ), f"the pane border is not on the seam: {rows[y][SIDEBAR_W]!r}"
    assert (
        str(screen.buffer[y][SIDEBAR_W + 1].bg) == "default"
    ), f"panel background bled into the pane interior: {screen.buffer[y][SIDEBAR_W + 1]!r}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_seam_has_no_background_bleed")


def test_all_three_edges_corner_ownership():
    """Verticals own the corners; the bottom sidebar spans only between them.

    Design-doc assertion 9. The left (20) and right (16) sidebars run the full
    terminal height, so the bottom sidebar (5 rows) is inset to the content
    columns and its panel starts at the content origin, not at column 0.
    """
    env = make_env(SIDEBAR_CFG_THREE)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    rows = screen.display

    left_w, right_w = 20, 16
    # The verticals are painted at the terminal's edges, from row 0.
    assert rows[0].startswith("Placeholder"), f"left panel missing: {rows[0][:24]!r}"
    assert (
        rows[0][COLS - right_w :].startswith("Placeholder")
    ), f"right panel missing: {rows[0][COLS - right_w:]!r}"

    # The bottom sidebar occupies the last 5 rows, inset between the verticals:
    # its panel starts at the content origin and never claims a corner.
    band = rows[ROWS - 5]
    assert band[:left_w].strip() == "", f"bottom sidebar claimed the left corner: {band!r}"
    assert (
        band[COLS - right_w :].strip() == ""
    ), f"bottom sidebar claimed the right corner: {band!r}"
    assert (
        band.index("Placeholder") == left_w
    ), f"bottom panel not at the content origin: {band!r}"

    # And the content shrank vertically to make room: the server's frame and
    # status bar must both end above the bottom band.
    status = [i for i, r in enumerate(rows) if "[NORMAL]" in r]
    assert status, "status bar missing"
    assert (
        status[0] < ROWS - 5
    ), f"content still reaches into the bottom sidebar (status row {status[0]})"

    teardown(child, env)
    check_no_panic()
    print("PASS test_all_three_edges_corner_ownership")


# A distinct frame background makes the panel's fill visible to pyte, so the
# seam test can prove the panel painted its rect and not one column more.
FRAME_BG = "5f00af"
SIDEBAR_CFG_LEFT_THEMED = (
    SIDEBAR_CFG_LEFT
    + f"""
[appearance.theme]
frame_bg = "#{FRAME_BG}"
"""
)

SIDEBAR_CFG_RIGHT = """
[[sidebar]]
edge = "right"
size = 20

  [[sidebar.panel]]
  plugin = "placeholder"
"""


def test_resize_keeps_the_right_sidebar():
    """A right sidebar survives a terminal resize; content stops short of it.

    NOTE on what this does and does NOT pin. It does **not** discriminate the
    `set_origin`-without-`set_content_size` bug: with only the origin set, the
    next `render_full`'s end-of-row clear does reach the terminal's right edge
    and blank the panel, but `chrome.paint` repaints it in the same flush, so
    the screen ends up identical -- verified by injecting that bug, after which
    this test still passed. What it genuinely pins is the user-visible
    invariant: after a resize the panel is still there and the server's content
    is laid out for the reduced width.
    """
    env = make_env(SIDEBAR_CFG_RIGHT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    rows = screen.display
    assert any(
        "Placeholder" in r[COLS - 20 :] for r in rows
    ), f"right panel missing before resize:\n{rows[0]!r}"

    new_cols, new_rows = 90, 26
    screen.resize(new_rows, new_cols)
    child.setwinsize(new_rows, new_cols)
    pump(1.5)
    child.send(b"echo AFTERRESIZE\r")
    pump(1.5)
    rows = screen.display

    assert any(
        "Placeholder" in r[new_cols - 20 :] for r in rows
    ), "the resize blanked the right sidebar:\n" + "\n".join(rows[:4])
    hit = [r for r in rows if "AFTERRESIZE" in r]
    assert hit, "shell output never appeared after the resize"
    for r in hit:
        assert (
            r.index("AFTERRESIZE") + len("AFTERRESIZE") <= new_cols - 20
        ), f"content ran into the right sidebar: {r!r}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_resize_keeps_the_right_sidebar")


def test_overlay_teardown_restores_the_sidebar():
    """Closing a modal overlay must not erase the panels.

    Overlays paint OVER the screen and are torn down with `clear_overlay`,
    which repaints the whole front buffer. Panels live in that buffer, so they
    have to come back -- and the overlay itself is chrome, so it centres on the
    whole terminal rather than on the content rect.
    """
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    # Prefix (Ctrl-a) then `x m` opens the session manager.
    child.send(b"\x01")
    child.send(b"x")
    child.send(b"m")
    pump(1.2)
    assert any(
        "Session Manager" in r for r in screen.display
    ), "session manager never opened:\n" + "\n".join(screen.display[:4])
    child.send(b"\x1b")
    pump(1.2)
    rows = screen.display
    assert not any("Session Manager" in r for r in rows), "overlay never closed"
    assert any(
        "Placeholder" in r[:SIDEBAR_W] for r in rows
    ), "overlay teardown erased the sidebar:\n" + "\n".join(rows[:4])
    # `repaint_all` re-renders the whole front buffer with `show_cursor=false`
    # and must put `last_cursor` back afterwards; without that restore the next
    # `paint_panel` faithfully re-hides a cursor the overlay teardown had just
    # shown, and the shell is left with no cursor for the rest of the session.
    assert screen.cursor.hidden is False, "overlay teardown left the cursor hidden"
    assert (
        screen.cursor.x >= SIDEBAR_W
    ), f"the cursor is parked inside the sidebar at column {screen.cursor.x}"
    row = screen.display[screen.cursor.y]
    assert (
        row[screen.cursor.x - 2 : screen.cursor.x] == "> "
    ), f"the cursor is not parked at the shell prompt: {row!r} x={screen.cursor.x}"
    teardown(child, env)
    check_no_panic()
    print("PASS test_overlay_teardown_restores_the_sidebar")


def test_cursor_stays_visible_with_a_sidebar():
    """The hardware cursor must survive `chrome.paint`.

    `paint_panel` hides the cursor while it draws. The frame arms run
    `render_*` -> `chrome.paint` -> `relay_overlays` -> one `flush`, so the
    panel's Hide and the frame's Show land in the SAME flush: unless the panel
    painter re-issues the cursor, a configured sidebar leaves the shell with no
    cursor, permanently. This asserts the cursor is shown and parked at the
    shell prompt inside the content rect.
    """
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(b"echo CURSORHERE\r")
    pump(1.8)
    assert (
        screen.cursor.hidden is False
    ), "the sidebar left the hardware cursor hidden"
    assert (
        screen.cursor.x >= SIDEBAR_W
    ), f"the cursor is parked inside the sidebar at column {screen.cursor.x}"
    assert (
        "CURSORHERE" in "".join(screen.display)
    ), "the shell never echoed; the cursor position proves nothing"
    # `PS1` is pinned to "> " by make_env, so the cursor must sit immediately
    # after a prompt -- not merely somewhere right of the seam.
    row = screen.display[screen.cursor.y]
    assert (
        row[screen.cursor.x - 2 : screen.cursor.x] == "> "
    ), f"the cursor is not parked at the shell prompt: {row!r} x={screen.cursor.x}"
    teardown(child, env)
    check_no_panic()
    print("PASS test_cursor_stays_visible_with_a_sidebar")


def test_search_highlights_land_on_the_match():
    """`render_search_highlight` takes a CONTENT-relative pane rect.

    It indexes the absolute front buffer and issues absolute `MoveTo`s, so
    without the origin every highlight lands `origin.x` columns too far left --
    inside the sidebar. Assert every highlighted cell is right of the seam and
    carries a character of the needle, i.e. the highlight is ON the match.
    """
    needle = "NEEDLEXYZ"
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(f"echo {needle}\r".encode())
    pump(1.5)
    # Prefix (Ctrl-a) then `s s` enters search; type the needle and confirm.
    child.send(b"\x01ss")
    pump(0.8)
    child.send(needle.encode())
    pump(0.8)
    child.send(b"\r")
    pump(1.5)

    # Two rows carry a background of their own and are not search highlights:
    # the status bar (last row) and the search prompt, which is chrome and
    # deliberately spans the whole terminal.
    skip = {ROWS - 1} | {
        y for y in range(ROWS) if screen.display[y].lstrip().startswith("/" + needle)
    }
    highlighted = [
        (x, y)
        for y in range(ROWS)
        for x in range(COLS)
        if y not in skip and str(screen.buffer[y][x].bg) != "default"
    ]
    assert highlighted, "search produced no highlights"
    bad = [(x, y) for (x, y) in highlighted if x < SIDEBAR_W]
    assert not bad, f"search highlighted sidebar columns: {bad[:8]}"
    off = [
        (x, y, screen.buffer[y][x].data)
        for (x, y) in highlighted
        if screen.buffer[y][x].data not in set(needle)
    ]
    assert not off, f"highlight is not sitting on the match: {off[:8]}"

    child.send(b"\x1b")
    pump(0.4)
    teardown(child, env)
    check_no_panic()
    print("PASS test_search_highlights_land_on_the_match")


def test_yank_copies_content_not_the_sidebar():
    """`extract_text` reads the ABSOLUTE front buffer at pane coordinates.

    The pane offset arrives content-relative, so without the origin a yank
    lifts whatever sits in the sidebar's columns straight into the user's
    clipboard. Type a token WITHOUT pressing Enter so it is on the cursor's own
    line, select that line in visual mode, and assert the OSC 52 write carries
    the shell line -- not the panel's text or a run of blanks.
    """
    token = "YANKTOKEN"
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(f"echo {token}".encode())
    pump(1.2)
    child.send(b"\x01v")  # visual (copy) mode
    pump(0.8)
    for _ in range(25):  # `h` clamps at column 0 of the pane
        child.send(b"h")
        time.sleep(0.06)
    pump(0.6)
    child.send(b"v")  # start a character selection
    pump(0.5)
    for _ in range(len("> echo ") + len(token)):
        child.send(b"l")
        time.sleep(0.08)
    pump(0.6)
    child.send(b"y")
    pump(1.2)

    got = yanks(raw)
    assert got, "y produced no OSC 52 clipboard write"
    yanked = got[-1]
    assert token in yanked, (
        f"the yank did not copy the shell line: {yanked!r} "
        "(a sidebar-relative read would copy the panel or blanks)"
    )
    assert "Placeholder" not in yanked and "idle" not in yanked, (
        f"the yank copied the sidebar panel: {yanked!r}"
    )

    child.send(b"\x1b")
    pump(0.4)
    teardown(child, env)
    check_no_panic()
    print("PASS test_yank_copies_content_not_the_sidebar")


def snapshot(screen):
    """Per-cell (char, fg, bg, reverse) so a colour-only change is visible."""
    return [
        [
            (
                screen.buffer[y][x].data,
                screen.buffer[y][x].fg,
                screen.buffer[y][x].bg,
                screen.buffer[y][x].reverse,
            )
            for x in range(COLS)
        ]
        for y in range(ROWS)
    ]


def test_visual_selection_highlights_correct_columns():
    """Visual mode must paint into content columns, never sidebar columns.

    `render_visual_overlay` draws the block cursor and the selection at
    `pane_offset + col`, and the server reports that offset CONTENT-relative.
    At a non-zero content origin an un-offset overlay lands `origin.x` columns
    too far left -- inside the sidebar (design doc section 7, crossing 3).

    Detected by diffing the screen across the mode switch: the only cells that
    may change are the status row (NORMAL -> VISUAL) and the block cursor, and
    the block cursor must be at or right of the seam. `PS1` is pinned short by
    `make_env` so the cursor sits within the sidebar's columns if unoffset.
    """
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump, raw = spawn(env)
    pump(1.5)
    child.send(b"echo VISUALTARGET\r")
    pump(1.2)
    before = snapshot(screen)
    # Enter visual mode: prefix (Ctrl-a) then v.
    child.send(b"\x01v")
    pump(1.2)
    after = snapshot(screen)

    changed = [
        (x, y) for y in range(ROWS) for x in range(COLS) if before[y][x] != after[y][x]
    ]
    # The status row flips on its own; the block cursor is the overlay.
    overlay = [(x, y) for (x, y) in changed if y != ROWS - 1]
    assert overlay, f"visual overlay never painted; changed={changed[:8]}"
    bad = [(x, y) for (x, y) in overlay if x < SIDEBAR_W]
    assert not bad, f"visual overlay painted into the sidebar: {bad[:8]}"

    child.send(b"\x1b")
    pump(0.4)
    teardown(child, env)
    check_no_panic()
    print("PASS test_visual_selection_highlights_correct_columns")


if __name__ == "__main__":
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    test_no_sidebar_is_unchanged()
    test_left_sidebar_offsets_content()
    test_seam_has_no_background_bleed()
    test_all_three_edges_corner_ownership()
    test_resize_keeps_the_right_sidebar()
    test_overlay_teardown_restores_the_sidebar()
    test_visual_selection_highlights_correct_columns()
    test_cursor_stays_visible_with_a_sidebar()
    test_search_highlights_land_on_the_match()
    test_yank_copies_content_not_the_sidebar()
    print("ALL PASS")
