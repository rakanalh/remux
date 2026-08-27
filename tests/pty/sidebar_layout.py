#!/usr/bin/env python3
"""Sidebar layout: content offset, seam integrity, corner ownership.

Covers the design doc's test-table rows 1, 2, 3, 8 and 9 plus the visual-mode
crossing from section 7.

Run: python3 tests/pty/sidebar_layout.py
"""
import os
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


def spawn(env):
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(ROWS, COLS), encoding=None)

    def pump(t=0.6):
        end = time.time() + t
        while time.time() < end:
            try:
                stream.feed(child.read_nonblocking(65536, 0.1))
            except Exception:
                pass

    return child, screen, pump


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
    child, screen, pump = spawn(env)
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
    child, screen, pump = spawn(env)
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

    # Assertion 2: no bleed at the seam -- the panel's block fill must stop
    # before the first content column.
    y = hit[0]
    assert screen.buffer[y][SIDEBAR_W].data != "█", "panel block bled past the seam"

    teardown(child, env)
    check_no_panic()
    print("PASS test_left_sidebar_offsets_content")


def test_all_three_edges_corner_ownership():
    """Verticals own the corners; the bottom sidebar spans only between them.

    Design-doc assertion 9. The left (20) and right (16) sidebars run the full
    terminal height, so the bottom sidebar (5 rows) is inset to the content
    columns and its panel starts at the content origin, not at column 0.
    """
    env = make_env(SIDEBAR_CFG_THREE)
    child, screen, pump = spawn(env)
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


SIDEBAR_CFG_RIGHT = """
[[sidebar]]
edge = "right"
size = 20

  [[sidebar.panel]]
  plugin = "placeholder"
"""


def test_resize_keeps_the_right_sidebar():
    """A terminal resize must re-point the origin AND the content size.

    `Renderer::resize` resets the content size to the whole terminal but
    deliberately leaves the origin alone. If the resize path sets only one of
    them, the end-of-row clear in the next `render_full` runs to the terminal's
    right edge and blanks a right sidebar -- so this asserts the panel survives
    a resize and the content still stops short of it.
    """
    env = make_env(SIDEBAR_CFG_RIGHT)
    child, screen, pump = spawn(env)
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
    child, screen, pump = spawn(env)
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
    teardown(child, env)
    check_no_panic()
    print("PASS test_overlay_teardown_restores_the_sidebar")


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
    child, screen, pump = spawn(env)
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
    test_all_three_edges_corner_ownership()
    test_resize_keeps_the_right_sidebar()
    test_overlay_teardown_restores_the_sidebar()
    test_visual_selection_highlights_correct_columns()
    print("ALL PASS")
