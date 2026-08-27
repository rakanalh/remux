#!/usr/bin/env python3
"""Sidebar navigation: directional keys reach sidebars like they reach panes.

Covers the design doc's test-table rows 7, 13 and 14:

  7  `Alt+h` from the leftmost pane focuses the left sidebar; `Alt+l` returns.
 13  Movement along a stacked sidebar's axis walks its panels.
 14  The bottom edge test runs against `pane_area` (the content rect minus
     the status-bar row), not the content rect, so `Alt+j` enters a bottom
     sidebar. See that test for why the `status_bar_position = "top"` half of
     row 14 is unit-tested rather than driven here.

Plus the two rules the whole design rests on: nothing inside a sidebar leaks a
`PaneFocus*` to the server, and with NO sidebar configured every one of these
keys behaves exactly as it did before.

The placeholder plugin renders `focused`/`idle` on its second row and bumps a
counter on `j`/`k`, which is how "which panel has the keyboard" and "did a
plain keystroke reach the plugin" are both observable.

EVERY assertion here except the explicit regression test runs with a sidebar
configured. With no `[[sidebar]]` the panel rects are empty and none of this
code is reachable, so a sidebar-less run would be structurally blind.

Run: python3 tests/pty/sidebar_nav.py
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
RUNDIR = "/tmp/rmx-sbn"
COLS, ROWS = 100, 30
SIDEBAR_W = 30
BOTTOM_H = 6

PANEL = """
  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

CFG_LEFT = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

CFG_LEFT_TWO_PANELS = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}{PANEL}"""

CFG_BOTTOM = f"""
[[sidebar]]
edge = "bottom"
size = {BOTTOM_H}
visible = true
{PANEL}"""

# A CUSTOM multi-command chain. `Prefix h` does NOT reach the ExecuteChain arm
# -- `execute_action_chain` handles `EnterNormal` as a mode change rather than a
# command, so the default `["PaneFocusLeft", "EnterNormal"]` collapses to a
# single `Execute`. Only a chain of two SERVER commands gets there.
CFG_CHAIN = f"""
[keybindings.command]
"Alt-4" = "PaneFocusRight; PaneFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

# A sidebar that cannot fit: 30 columns wanted, but `MIN_CONTENT_COLS` is 20,
# so at a 20-column terminal `effective_sizes` force-hides it entirely.
CFG_TOO_NARROW = f"""
[keybindings.command]
"Alt-2" = "SidebarFocusLeft"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

# The regression baseline: a valid config with no `[[sidebar]]` at all, so
# `panel_rects` is empty and every sidebar code path is unreachable.
# The toggle/focus/cycle actions bound to keys, so the `InputAction::Sidebar`
# path is exercised end to end rather than only through directional keys.
CFG_ACTIONS = f"""
[keybindings.command]
"Alt-1" = "SidebarToggleLeft"
"Alt-2" = "SidebarFocusLeft"
"Alt-3" = "SidebarCycle"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}{PANEL}"""

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


def server_log() -> str:
    path = f"{RUNDIR}/state/remux/server.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def client_log() -> str:
    path = f"{RUNDIR}/state/remux/client.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def pane_focus_cmds(log: str):
    """Every `PaneFocus*` command the SERVER was asked to run."""
    return re.findall(r"msg=Command\((PaneFocus\w+)\)", log)


# Left sidebar column band, and the rows the placeholder's markers live on.
def markers(screen, x0=0, x1=SIDEBAR_W):
    """(row_index, "focused"/"idle") for every panel marker on screen."""
    out = []
    for y, row in enumerate(screen.display):
        cell = row[x0:x1]
        if "focused" in cell:
            out.append((y, "focused"))
        elif "idle" in cell:
            out.append((y, "idle"))
    return out


def focused_rows(screen, x0=0, x1=SIDEBAR_W):
    return [y for (y, m) in markers(screen, x0, x1) if m == "focused"]


ALT_H, ALT_J, ALT_K, ALT_L = b"\x1bh", b"\x1bj", b"\x1bk", b"\x1bl"


def test_alt_h_enters_the_left_sidebar_and_alt_l_returns():
    """Row 7. And the return trip must hand the keyboard back to the shell."""
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    assert markers(screen) == [(1, "idle")], (
        f"the panel did not start unfocused: {markers(screen)}"
    )
    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), (
        "Alt+h from the leftmost pane did not focus the sidebar:\n"
        + "\n".join(screen.display[:4])
    )
    leaked = pane_focus_cmds(server_log())
    assert not leaked, f"Alt+h was forwarded to the server as {leaked}"

    # Further left from inside a LEFT sidebar has nowhere to go. That is a
    # swallowed no-op, never a leaked `PaneFocusLeft`: the direction is not
    # along the stack axis, so it is the one that falls through if the
    # "nothing inside a sidebar reaches the server" rule is written as a
    # conditional instead of an unconditional `true`.
    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), (
        f"a second Alt+h moved focus out of the sidebar: {markers(screen)}"
    )
    leaked = pane_focus_cmds(server_log())
    assert not leaked, (
        f"Alt+h from INSIDE the left sidebar leaked to the server as {leaked}"
    )

    child.send(ALT_L)
    pump(1.0)
    assert not focused_rows(screen), (
        f"Alt+l did not leave the sidebar: {markers(screen)}"
    )
    # The keyboard came back with the focus.
    child.send(b"echo nav1\r")
    pump(1.2)
    assert any("nav1" in r[SIDEBAR_W:] for r in screen.display), (
        "after leaving the sidebar a keystroke never reached the shell:\n"
        + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_alt_h_enters_the_left_sidebar_and_alt_l_returns")


def test_a_focused_panel_takes_plain_keys():
    """A focused sidebar owns the keyboard: `j` goes to the plugin, not the PTY."""
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"
    before = [r[:SIDEBAR_W] for r in screen.display]
    child.send(b"j")
    pump(1.0)
    after = [r[:SIDEBAR_W] for r in screen.display]
    assert before != after, (
        "a plain key with the sidebar focused never reached the plugin"
    )
    assert not any("j" in r[SIDEBAR_W:] for r in screen.display), (
        "the key leaked to the shell as well:\n" + "\n".join(screen.display)
    )

    # Escape releases the keyboard.
    child.send(b"\x1b")
    pump(1.0)
    assert not focused_rows(screen), f"Escape did not leave the panel: {markers(screen)}"
    child.send(b"echo nav2\r")
    pump(1.2)
    assert any("nav2" in r[SIDEBAR_W:] for r in screen.display), (
        "Escape did not hand the keyboard back:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_focused_panel_takes_plain_keys")


def test_alt_j_and_alt_k_walk_stacked_panels():
    """Row 13, plus the no-op rule: past the last panel nothing is forwarded."""
    env = make_env(CFG_LEFT_TWO_PANELS)
    child, screen, pump = spawn(env)
    pump(1.5)

    assert len(markers(screen)) == 2, (
        f"expected two stacked panels, saw {markers(screen)}"
    )
    child.send(ALT_H)
    pump(1.0)
    first = focused_rows(screen)
    assert first == [1], f"Alt+h did not focus the top panel: {markers(screen)}"

    child.send(ALT_J)
    pump(1.0)
    second = focused_rows(screen)
    assert second and second != first, (
        f"Alt+j did not walk to the next panel: {markers(screen)}"
    )

    # Past the last panel: swallowed, focus unchanged, nothing reaches the server.
    child.send(ALT_J)
    pump(1.0)
    assert focused_rows(screen) == second, (
        f"Alt+j past the last panel moved focus: {markers(screen)}"
    )

    child.send(ALT_K)
    pump(1.0)
    assert focused_rows(screen) == first, (
        f"Alt+k did not walk back up: {markers(screen)}"
    )
    # And past the first panel is likewise a swallowed no-op.
    child.send(ALT_K)
    pump(1.0)
    assert focused_rows(screen) == first, (
        f"Alt+k past the first panel moved focus: {markers(screen)}"
    )

    leaked = pane_focus_cmds(server_log())
    assert not leaked, f"directional keys inside the sidebar leaked to the server: {leaked}"

    teardown(child, env)
    check_no_panic()
    print("PASS test_alt_j_and_alt_k_walk_stacked_panels")


def test_the_bottom_edge_is_measured_against_the_pane_area():
    """Row 14: the edge test runs against `pane_area`, not the content rect.

    The status bar takes one row of what the server composites, so the bottom
    pane's interior stops TWO rows short of the content rect's last row (the
    status row plus the border). Measuring against the content rect instead of
    `pane_area` misses by exactly that row and `Alt+j` never enters the bottom
    sidebar -- probed by swapping `pane_area` for `content_rect`.

    NOTE on the other half of spec row 14: `status_bar_position = "top"` is
    currently parsed and validated but honoured by NOTHING -- the server always
    composites the bar at the bottom (no `StatusBarPosition` reference exists
    outside `src/client/chrome/` and `src/config/`). A PTY test cannot observe
    the top case until that is wired up, so `pane_area`'s `Top` arithmetic is
    covered by the unit tests in `src/client/chrome/mod.rs` alone.
    """
    env = make_env(CFG_BOTTOM)
    child, screen, pump = spawn(env)
    pump(1.5)

    assert markers(screen, 0, COLS), "the bottom panel never painted"
    child.send(ALT_J)
    pump(1.0)
    assert focused_rows(screen, 0, COLS), (
        "Alt+j did not enter the bottom sidebar:\n"
        + "\n".join(screen.display[-8:])
    )
    leaked = pane_focus_cmds(server_log())
    assert not leaked, f"Alt+j was forwarded to the server as {leaked}"

    # `Alt+k` is the way back out of a bottom sidebar.
    child.send(ALT_K)
    pump(1.0)
    assert not focused_rows(screen, 0, COLS), (
        f"Alt+k did not leave the bottom sidebar: {markers(screen, 0, COLS)}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_bottom_edge_is_measured_against_the_pane_area")


def test_a_non_edge_pane_still_forwards_to_the_server():
    """Interception is only at the edge: an inner pane's Alt+h reaches the server.

    Runs with a sidebar configured, so it exercises the `!at_edge` branch --
    the one that keeps normal pane navigation working when sidebars exist.
    """
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    # Split so there is a pane whose x is not 0.
    child.send(b"\x01")  # prefix
    child.send(b"p")
    child.send(b"v")  # pane -> split vertical
    pump(1.5)
    child.send(ALT_H)
    pump(1.0)

    got = pane_focus_cmds(server_log())
    assert "PaneFocusLeft" in got, (
        "Alt+h from the RIGHT pane of a split was swallowed instead of "
        f"forwarded: {got}\n" + "\n".join(screen.display[:4])
    )
    assert not focused_rows(screen), (
        f"a non-edge Alt+h focused the sidebar: {markers(screen)}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_non_edge_pane_still_forwards_to_the_server")


def test_the_prefix_still_works_from_inside_a_panel():
    """A focused sidebar owns the keyboard but never the prefix.

    Without letting the prefix (and the chord keys that follow it) through,
    command mode would be unreachable from a panel.
    """
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(b"\x01")  # prefix
    pump(0.8)
    child.send(b"x")
    child.send(b"m")  # session manager
    pump(1.5)
    assert any("Session Manager" in r for r in screen.display), (
        "the prefix chord did not reach command mode from a focused panel:\n"
        + "\n".join(screen.display)
    )
    child.send(b"\x1b")
    pump(1.0)
    assert child.isalive(), "the client died leaving the session manager"

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_prefix_still_works_from_inside_a_panel")


def test_cycling_in_and_out_of_every_panel_leaves_the_client_alive():
    """Hammer the focus transitions; the client must survive with no panic."""
    env = make_env(CFG_LEFT_TWO_PANELS)
    child, screen, pump = spawn(env)
    pump(1.5)

    for _ in range(4):
        for key in (ALT_H, ALT_J, ALT_J, ALT_K, ALT_K, ALT_L):
            child.send(key)
            pump(0.15)
    pump(1.0)

    assert child.isalive(), "the client died cycling sidebar focus"
    assert not focused_rows(screen), (
        f"the cycle did not end back on the content: {markers(screen)}"
    )
    child.send(b"echo nav3\r")
    pump(1.2)
    assert any("nav3" in r[SIDEBAR_W:] for r in screen.display), (
        "the keyboard was stranded after cycling:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_cycling_in_and_out_of_every_panel_leaves_the_client_alive")


def test_with_no_sidebar_every_directional_key_is_unchanged():
    """The regression gate: with no `[[sidebar]]`, nothing new fires."""
    env = make_env(CFG_NONE)
    child, screen, pump = spawn(env)
    pump(1.5)

    for key in (ALT_H, ALT_J, ALT_K, ALT_L):
        child.send(key)
        pump(0.3)
    pump(1.0)

    got = pane_focus_cmds(server_log())
    for want in ("PaneFocusLeft", "PaneFocusDown", "PaneFocusUp", "PaneFocusRight"):
        assert want in got, f"{want} was not forwarded with no sidebar configured: {got}"
    assert child.isalive(), "the client died with no sidebar configured"
    child.send(b"echo nav4\r")
    pump(1.2)
    assert any("nav4" in r for r in screen.display), (
        "keystrokes stopped reaching the shell with no sidebar configured:\n"
        + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_with_no_sidebar_every_directional_key_is_unchanged")


def test_the_sidebar_actions_toggle_focus_and_cycle():
    """`SidebarToggleLeft` / `SidebarFocusLeft` / `SidebarCycle`, bound to keys.

    Toggling is the one that moves the CONTENT RECT, so it is checked on screen:
    with the sidebar hidden the server's frame must start at column 0, which
    only happens if the new rect was synced AND sent as a `Resize`.
    """
    env = make_env(CFG_ACTIONS)
    child, screen, pump = spawn(env)
    pump(1.5)

    assert len(markers(screen)) == 2, f"expected two panels: {markers(screen)}"
    assert screen.display[0][:SIDEBAR_W].strip(), "the sidebar never painted"

    # Focus, then cycle through both panels and back out to the content.
    child.send(b"\x1b2")
    pump(1.0)
    first = focused_rows(screen)
    assert first, f"SidebarFocusLeft did not focus a panel: {markers(screen)}"

    child.send(b"\x1b3")
    pump(1.0)
    second = focused_rows(screen)
    assert second and second != first, (
        f"SidebarCycle did not move to the next panel: {markers(screen)}"
    )
    child.send(b"\x1b3")
    pump(1.0)
    assert not focused_rows(screen), (
        f"SidebarCycle did not return to the content: {markers(screen)}"
    )

    # Toggle off: the panels go, and the server's frame moves to column 0.
    child.send(b"\x1b1")
    pump(1.5)
    assert not markers(screen), f"the sidebar did not hide: {markers(screen)}"
    assert screen.display[0].startswith("\u256d"), (
        "the content rect did not grow into the freed columns:\n"
        + "\n".join(screen.display[:3])
    )

    # Toggle back on.
    child.send(b"\x1b1")
    pump(1.5)
    assert len(markers(screen)) == 2, (
        f"the sidebar did not come back: {markers(screen)}"
    )
    assert not screen.display[0].startswith("\u256d"), (
        "the content rect did not shrink again:\n" + "\n".join(screen.display[:3])
    )

    child.send(b"echo nav5\r")
    pump(1.2)
    assert any("nav5" in r[SIDEBAR_W:] for r in screen.display), (
        "the keyboard was stranded after toggling:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_sidebar_actions_toggle_focus_and_cycle")


def test_the_prefix_chord_also_enters_the_sidebar():
    """`Prefix p h` -- the command-mode route to `PaneFocusLeft`.

    Every other test here drives the Alt shortcuts. This one comes through the
    keybinding tree instead, with the which-key popup on screen when the
    interception fires: the popup must be torn down AND the sidebar focused,
    which is why the interception sits after the which-key hide rather than
    before it.
    """
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(b"\x01")  # prefix
    child.send(b"p")  # the Pane group, where the directional leaves live
    pump(1.0)
    assert any("focus left" in r for r in screen.display), (
        "the which-key popup did not open; the teardown assertion would be "
        "vacuous:\n" + "\n".join(screen.display)
    )
    child.send(b"h")
    pump(1.2)

    assert focused_rows(screen), (
        "Prefix p h did not focus the sidebar:\n" + "\n".join(screen.display[:6])
    )
    leaked = pane_focus_cmds(server_log())
    assert not leaked, f"Prefix p h was forwarded to the server as {leaked}"
    assert not any("focus left" in r for r in screen.display), (
        "the which-key popup survived the interception:\n"
        + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_prefix_chord_also_enters_the_sidebar")


def test_a_command_chain_intercepts_per_command():
    """The `ExecuteChain` arm: one command is swallowed, the other forwards.

    `Alt-4` is bound to `PaneFocusRight; PaneFocusLeft`. There is no right
    sidebar, so the first command must reach the server unchanged; the second
    must be swallowed by the left sidebar. Asserting both directions is what
    makes this a test of the chain arm rather than of chains in general.
    """
    env = make_env(CFG_CHAIN)
    child, screen, pump = spawn(env)
    pump(1.5)

    assert markers(screen), "the panel never painted"
    child.send(b"\x1b4")
    pump(1.5)

    got = pane_focus_cmds(server_log())
    assert "PaneFocusRight" in got, (
        f"the chain's non-intercepted command never reached the server: {got}"
    )
    assert "PaneFocusLeft" not in got, (
        f"the chain's intercepted command leaked to the server: {got}"
    )
    assert focused_rows(screen), (
        f"the chain did not focus the sidebar: {markers(screen)}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_command_chain_intercepts_per_command")


def test_focusing_a_sidebar_that_cannot_fit_is_refused_and_logged():
    """`focus_edge` must not park focus on a panel nobody can see.

    At 20 columns a 30-wide sidebar is force-hidden by `effective_sizes`, so
    `SidebarFocusLeft` has nothing to focus. Focusing it anyway would swallow
    every keystroke into an invisible panel -- the Step 0 trap in a second
    disguise. The refusal is logged at debug so it is diagnosable from
    `client.log` rather than looking like a dropped keypress.
    """
    env = make_env(CFG_TOO_NARROW)
    child, screen, pump = spawn(env, cols=20, rows=10)
    pump(1.5)

    assert not markers(screen, 0, 20), (
        f"the sidebar was not force-hidden at 20 columns: {markers(screen, 0, 20)}"
    )
    child.send(b"\x1b2")
    pump(1.2)

    log = client_log()
    assert "focus_edge(Left) refused" in log, (
        "the refusal was not logged, so it is indistinguishable from a dropped "
        "keypress:\n" + "\n".join(
            ln for ln in log.splitlines() if "sidebar:" in ln
        )
    )
    assert child.isalive(), "the client died refusing to focus a hidden sidebar"

    # Focus stayed on the content: the keyboard still works.
    child.send(b"echo nav6\r")
    pump(1.5)
    assert any("nav6" in r for r in screen.display), (
        "the keyboard was swallowed by the invisible panel:\n"
        + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_focusing_a_sidebar_that_cannot_fit_is_refused_and_logged")


if __name__ == "__main__":
    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    test_alt_h_enters_the_left_sidebar_and_alt_l_returns()
    test_a_focused_panel_takes_plain_keys()
    test_alt_j_and_alt_k_walk_stacked_panels()
    test_the_bottom_edge_is_measured_against_the_pane_area()
    test_a_non_edge_pane_still_forwards_to_the_server()
    test_the_prefix_still_works_from_inside_a_panel()
    test_cycling_in_and_out_of_every_panel_leaves_the_client_alive()
    test_the_prefix_chord_also_enters_the_sidebar()
    test_a_command_chain_intercepts_per_command()
    test_the_sidebar_actions_toggle_focus_and_cycle()
    test_focusing_a_sidebar_that_cannot_fit_is_refused_and_logged()
    test_with_no_sidebar_every_directional_key_is_unchanged()
    print("ALL PASS")
