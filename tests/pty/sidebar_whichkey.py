#!/usr/bin/env python3
"""The which-key popup is torn down by a sidebar action that changes nothing.

Reported: `Prefix b h` with no left sidebar leaves the popup on screen for
good, and the next `h` is typed into the shell UNDERNEATH it.

The `InputAction::Sidebar` arm had no `whichkey` teardown at all. It got away
with it whenever the intent moved the content rect: that sends a `Resize`, the
server answers with a `FullRender`, and the full repaint incidentally erased
the popup. Every intent that changes NOTHING repaints nothing -- `Chrome::paint`
over zero panel rects emits zero bytes -- so the popup survived while the mode
reset underneath it handed the next keystroke to the shell.

Three no-op intents are covered, plus the cursor: `clear_overlay` replays the
front buffer with the cursor hidden, and with no panel painted afterwards
nothing puts it back.

The first two tests run WITH a sidebar configured (on the other edge, so the
pressed toggle is still a no-op) -- with none, `panel_rects` is empty and whole
paths are unreachable. The third is the user's literal repro, no sidebar at all.

Run: python3 tests/pty/sidebar_whichkey.py
"""
import os
import shutil
import subprocess
import sys
import time

import pexpect
import pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = "/tmp/rmx-sbw"
COLS, ROWS = 100, 30
SIDEBAR_W = 24

PANEL = """
  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

# A RIGHT sidebar. `Prefix b h` (toggle LEFT) then finds nothing to toggle --
# the reported no-op -- while the panel rects are non-empty, so the paint and
# cursor paths this exercises are the real ones.
CFG_RIGHT = f"""
[[sidebar]]
edge = "right"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

# No `[[sidebar]]` at all: the user's literal repro.
CFG_NONE = """
[appearance]
border_style = "zellij_style"
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


def popup_visible(screen) -> bool:
    """The Sidebar which-key popup, identified by leaves nothing else draws."""
    body = "\n".join(screen.display)
    return "toggle left" in body or "cycle focus" in body


def open_sidebar_group(child, screen, pump):
    child.send(b"\x01")  # prefix
    pump(0.5)
    child.send(b"b")
    pump(0.8)
    assert popup_visible(screen), (
        "the Sidebar which-key popup never opened:\n" + "\n".join(screen.display)
    )


def assert_shell_is_reachable(child, screen, pump, token):
    child.send(f"echo {token}\r".encode())
    pump(1.4)
    assert any(token in r for r in screen.display), (
        f"the shell never echoed {token}; the keyboard was stranded:\n"
        + "\n".join(screen.display)
    )


def run_case(cfg, key, token, name, escape_first=False):
    env = make_env(cfg)
    child, screen, pump = spawn(env)
    pump(1.6)

    open_sidebar_group(child, screen, pump)
    child.send(key)
    pump(1.2)

    assert not popup_visible(screen), (
        f"{name}: the popup survived a no-op sidebar action:\n"
        + "\n".join(screen.display)
    )
    assert not screen.cursor.hidden, (
        f"{name}: the terminal cursor was left hidden by the overlay teardown"
    )
    assert child.isalive(), f"{name}: the client died"

    # The reported second half: the next keystroke lands in the shell. It did
    # so even with the bug -- the point is that it now does so with the popup
    # actually gone. A cycle deliberately hands the keyboard to the panel, so
    # step back out to the content area first.
    if escape_first:
        child.send(b"\x1b")
        pump(0.6)
    assert_shell_is_reachable(child, screen, pump, token)

    teardown(child, env)
    check_no_panic()
    print(f"PASS {name}")


def test_a_toggle_for_an_unconfigured_edge_closes_the_popup():
    """`Prefix b h` with a sidebar on the RIGHT edge only: `toggle_edge`
    returns false, the content rect does not move, and nothing repaints."""
    run_case(CFG_RIGHT, b"h", "WK_TOGGLE", "test_a_toggle_for_an_unconfigured_edge_closes_the_popup")


def test_a_focus_cycle_that_moves_no_geometry_closes_the_popup():
    """`Prefix b b` (cycle) only moves focus, so the content rect never
    changes and the incidental `FullRender` never arrives."""
    run_case(
        CFG_RIGHT,
        b"b",
        "WK_CYCLE",
        "test_a_focus_cycle_that_moves_no_geometry_closes_the_popup",
        escape_first=True,
    )


def test_the_literal_repro_with_no_sidebar_configured():
    """`Prefix b h` with no `[[sidebar]]` at all -- exactly as reported."""
    run_case(CFG_NONE, b"h", "WK_NONE", "test_the_literal_repro_with_no_sidebar_configured")


def test_a_toggle_that_does_move_the_content_rect_still_closes_the_popup():
    """The path that already worked by accident must keep working now that the
    teardown is explicit -- and the sidebar must actually toggle."""
    name = "test_a_toggle_that_does_move_the_content_rect_still_closes_the_popup"
    env = make_env(CFG_RIGHT)
    child, screen, pump = spawn(env)
    pump(1.6)

    assert any("idle" in r[COLS - SIDEBAR_W:] for r in screen.display), (
        "the right panel did not paint at startup:\n" + "\n".join(screen.display)
    )

    open_sidebar_group(child, screen, pump)
    child.send(b"l")  # toggle right -> hides the configured sidebar
    pump(1.4)

    assert not popup_visible(screen), (
        f"{name}: the popup survived a real toggle:\n" + "\n".join(screen.display)
    )
    assert not any("idle" in r[COLS - SIDEBAR_W:] for r in screen.display), (
        f"{name}: the right sidebar did not hide:\n" + "\n".join(screen.display)
    )
    assert not screen.cursor.hidden, f"{name}: the cursor was left hidden"
    assert_shell_is_reachable(child, screen, pump, "WK_REAL")

    teardown(child, env)
    check_no_panic()
    print(f"PASS {name}")


if __name__ == "__main__":
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    test_a_toggle_for_an_unconfigured_edge_closes_the_popup()
    test_a_focus_cycle_that_moves_no_geometry_closes_the_popup()
    test_the_literal_repro_with_no_sidebar_configured()
    test_a_toggle_that_does_move_the_content_rect_still_closes_the_popup()
    print("ALL PASS")
