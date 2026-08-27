#!/usr/bin/env python3
"""Sidebar mouse routing: panels swallow their clicks, content is translated.

Covers the design doc's test-table rows 4 and 5, plus the drag rule the task
calls out as the part most likely to be subtly wrong: a gesture belongs to the
region it STARTED in, in both directions.

Ground truth is the SERVER log, not the screen: a click that reaches the server
is logged as `server: MouseClick client_id=… x=… y=…` (src/server/daemon.rs),
and a drag as `server: MouseDrag … start=(x,y) end=(x,y)`. (The task brief
asserted `mouse: click at (…` against server.log -- that string is the CLIENT's
line in src/main.rs, so both are checked here and the server's is the one that
proves routing.)

EVERY test here runs with a sidebar configured. With no `[[sidebar]]` the panel
rects are empty and none of this code is reachable, so a sidebar-less run would
be structurally blind to what these assertions are for.

Run: python3 tests/pty/sidebar_mouse.py
"""
import os
import re
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath("target/debug/remux")
RUNDIR = "/tmp/rmx-sbm"
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
        PS1="> ",
    )
    return env


def spawn(env, cols=COLS, rows=ROWS):
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    child = pexpect.spawn(BIN, [], env=env, dimensions=(rows, cols), encoding=None)

    def pump(t=0.6):
        end = time.time() + t
        while time.time() < end:
            try:
                chunk = child.read_nonblocking(65536, 0.1)
            except Exception:
                continue
            stream.feed(chunk)

    return child, screen, pump


def teardown(child, env):
    try:
        child.close(force=True)
    except Exception:
        pass
    try:
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


def server_log() -> str:
    path = f"{RUNDIR}/state/remux/server.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def client_log() -> str:
    path = f"{RUNDIR}/state/remux/client.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def clicks(log: str):
    """Every `server: MouseClick` the server handled, as (x, y, release)."""
    return [
        (int(m.group(1)), int(m.group(2)), m.group(3) == "true")
        for m in re.finditer(
            r"server: MouseClick client_id=\d+ x=(\d+) y=(\d+) release=(\w+)", log
        )
    ]


def drags(log: str):
    """Every `server: MouseDrag` the server handled, as (sx, sy, ex, ey, final)."""
    return [
        (
            int(m.group(1)),
            int(m.group(2)),
            int(m.group(3)),
            int(m.group(4)),
            m.group(5) == "true",
        )
        for m in re.finditer(
            r"server: MouseDrag client_id=\d+ start=\((\d+),(\d+)\) "
            r"end=\((\d+),(\d+)\) is_final=(\w+)",
            log,
        )
    ]


def panel_rows(screen):
    """The placeholder's own rows, which carry its click counter."""
    return [r[:SIDEBAR_W] for r in screen.display if "focused" in r or "idle" in r]


# SGR mouse reports. Coordinates are 1-based, as a real terminal sends them.
def sgr_press(col, row):
    return f"\x1b[<0;{col};{row}M".encode()


def sgr_drag(col, row):
    return f"\x1b[<32;{col};{row}M".encode()


def sgr_release(col, row):
    return f"\x1b[<0;{col};{row}m".encode()


def test_click_in_sidebar_is_swallowed():
    """Row 4: a press inside a panel reaches the plugin and NOT the server."""
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    before = panel_rows(screen)
    assert before, "the panel never painted; the counter assertion is vacuous"
    child.send(sgr_press(5, 3))
    child.send(sgr_release(5, 3))
    pump(1.2)
    after = panel_rows(screen)
    assert before != after, (
        f"click inside the sidebar never reached the plugin: {before} -> {after}"
    )

    leaked = clicks(server_log())
    assert not leaked, f"sidebar click leaked to the server as {leaked}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_click_in_sidebar_is_swallowed")


def test_click_in_content_is_translated():
    """Row 5: a content press arrives in CONTENT coordinates, not screen ones."""
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    # 1-based screen column 41 -> 0-based 40 -> content column 10.
    # 1-based screen row 3     -> 0-based 2  -> content row 2 (left sidebar,
    # so the vertical origin is 0).
    child.send(sgr_press(SIDEBAR_W + 11, 3))
    child.send(sgr_release(SIDEBAR_W + 11, 3))
    pump(1.2)

    got = clicks(server_log())
    assert got, f"content click never reached the server:\n{server_log()[-1500:]}"
    assert (10, 2) in [(x, y) for (x, y, _) in got], (
        f"content click not translated to content coordinates: {got}"
    )
    assert "mouse: click at (10, 2)" in client_log(), (
        "the client logged the untranslated coordinate:\n"
        + "\n".join(
            ln for ln in client_log().splitlines() if "mouse: click at" in ln
        )
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_click_in_content_is_translated")


def test_drag_from_content_over_the_sidebar_stays_with_the_content():
    """A gesture that STARTED in the content keeps the content, clamped.

    Dragging left past the seam must not be reinterpreted as a panel
    interaction: the plugin must never see it, and the coordinates the server
    receives must clamp at content column 0 rather than wrapping through a u16
    underflow.
    """
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    before = panel_rows(screen)
    assert before, "the panel never painted"
    child.send(sgr_press(SIDEBAR_W + 21, 5))  # content col 20, row 4
    pump(0.3)
    child.send(sgr_drag(SIDEBAR_W + 5, 5))  # still content: col 4
    pump(0.3)
    child.send(sgr_drag(5, 5))  # over the panel -> clamps to col 0
    pump(0.3)
    child.send(sgr_release(5, 5))
    pump(1.2)

    assert panel_rows(screen) == before, (
        "a drag that began in the content reached the plugin: "
        f"{before} -> {panel_rows(screen)}"
    )
    got = drags(server_log())
    assert got, f"the drag never reached the server:\n{server_log()[-1500:]}"
    final = [d for d in got if d[4]]
    assert final, f"no final drag was sent on release: {got}"
    sx, sy, ex, ey, _ = final[-1]
    assert (sx, sy) == (20, 4), f"drag anchor not in content coordinates: {final[-1]}"
    assert (ex, ey) == (0, 4), (
        f"drag over the sidebar was not clamped to the content rect: {final[-1]}"
    )
    for d in got:
        assert d[2] < COLS - SIDEBAR_W, f"drag end escaped the content rect: {d}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_drag_from_content_over_the_sidebar_stays_with_the_content")


def test_drag_from_the_sidebar_over_the_content_stays_with_the_panel():
    """The mirror rule: a gesture that STARTED in a panel never reaches the server.

    The press bumps the placeholder's counter (so we know the panel took it);
    dragging out into the content and releasing there must still be swallowed.
    """
    env = make_env(SIDEBAR_CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    before = panel_rows(screen)
    assert before, "the panel never painted"
    child.send(sgr_press(5, 3))
    pump(0.5)
    assert panel_rows(screen) != before, "the press never reached the plugin"
    child.send(sgr_drag(SIDEBAR_W + 11, 5))
    pump(0.3)
    child.send(sgr_release(SIDEBAR_W + 21, 6))
    pump(1.2)

    log = server_log()
    assert not clicks(log), f"a panel-anchored gesture leaked clicks: {clicks(log)}"
    assert not drags(log), f"a panel-anchored gesture leaked drags: {drags(log)}"
    assert child.isalive(), "the client died handling a cross-region drag"

    # And the grab is released: a fresh content click still works afterwards.
    child.send(sgr_press(SIDEBAR_W + 11, 3))
    child.send(sgr_release(SIDEBAR_W + 11, 3))
    pump(1.2)
    got = clicks(server_log())
    assert (10, 2) in [(x, y) for (x, y, _) in got], (
        f"the panel kept the grab after release: {got}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_drag_from_the_sidebar_over_the_content_stays_with_the_panel")


if __name__ == "__main__":
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    test_click_in_sidebar_is_swallowed()
    test_click_in_content_is_translated()
    test_drag_from_content_over_the_sidebar_stays_with_the_content()
    test_drag_from_the_sidebar_over_the_content_stays_with_the_panel()
    print("ALL PASS")
