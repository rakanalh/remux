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

# A MIXED chain (a directional command plus a destructive one) and a group-prefix
# shortcut, for the two exemption edge cases the review found.
CFG_EXEMPTIONS = f"""
[keybindings.command]
"Alt-5" = "PaneFocusLeft; SetMaster"
"Alt-6" = "@p"

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

# The toggle action bound to a key with NO `[[sidebar]]` at all: the regression
# gate for persistence. A toggle that finds no sidebar changes nothing, so
# nothing may be written -- "exactly as today" includes not creating a state
# file a previous build never had.
CFG_ACTIONS_NO_SIDEBAR = """
[keybindings.command]
"Alt-1" = "SidebarToggleLeft"
"""

# `Resize*` bound to NORMAL-mode shortcuts, pure and mixed. A focused sidebar
# re-targets a pure resize at itself; a mixed chain earns no exemption and goes
# to the plugin like any other key, or its `SetMaster` half would reach the
# server while a panel has the keyboard.
# A `PaneFocus*` merged INTO the built-in sticky Resize group. A user-declared
# group is non-sticky, but merging into a built-in one preserves its
# stickiness (`merge_maps`), so these leaves arrive as
# `ExecuteAndShowWhichKey` -- the arm neither `Execute` nor `ExecuteChain`
# covers. Task 7's invariant is stated without qualification, so it has to hold
# here too.
CFG_STICKY_FOCUS = f"""
[keybindings.command.p.R]
x = "PaneFocusLeft"
y = "PaneFocusRight"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

CFG_ALT_RESIZE = f"""
[keybindings.command]
"Alt-7" = "ResizeRight 5"
"Alt-8" = "ResizeRight 5; SetMaster"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true
{PANEL}"""

# A RIGHT sidebar, closed. Paired with state written for the LEFT edge, this is
# what makes "unmatched state is ignored" observable on screen: an `apply` that
# matched positionally instead of by edge would open this bar to the left bar's
# saved size, and its panel would paint.
CFG_RIGHT_HIDDEN = f"""
[[sidebar]]
edge = "right"
size = 20
visible = false
{PANEL}"""


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


# How far into its bar a sidebar's panel content starts. The sidebar is framed
# in the session's border style (zellij by default: a box on all four sides), and
# the frame is drawn INSIDE the bar, so the panel's first row and first column
# are one cell in. The bar itself -- and so the content rect the server is sized
# for -- is unchanged, which is why only the marker ROWS below moved.
FRAME = 1


def pane_frame_column(screen, start=0):
    """Column where the SERVER's pane frame begins, at or after `start`.

    Row 0 now carries two box corners with a sidebar up: the sidebar's own
    frame at column 0 and the pane's at the seam. `startswith("\u256d")` used
    to mean "no sidebar"; it no longer does, so the tests below name the column
    they mean instead.
    """
    return screen.display[0].find("\u256d", start)


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

    assert markers(screen) == [(FRAME + 1, "idle")], (
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
    assert first == [FRAME + 1], f"Alt+h did not focus the top panel: {markers(screen)}"

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
    assert pane_frame_column(screen) == 0, (
        "the content rect did not grow into the freed columns:\n"
        + "\n".join(screen.display[:3])
    )

    # Toggle back on.
    child.send(b"\x1b1")
    pump(1.5)
    assert len(markers(screen)) == 2, (
        f"the sidebar did not come back: {markers(screen)}"
    )
    assert pane_frame_column(screen, SIDEBAR_W) == SIDEBAR_W, (
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


def bracketed_paste(text: str) -> bytes:
    """What a terminal sends for a paste once bracketed paste is enabled."""
    return b"\x1b[200~" + text.encode() + b"\x1b[201~"


def test_a_paste_does_not_leak_past_a_focused_sidebar():
    """Bracketed paste is a SEPARATE crossterm event from a key press.

    It never passes the key routing, so without an explicit guard Ctrl-Shift-V
    with a panel focused types straight into the shell behind the sidebar --
    the one path by which input reaches the server while a panel owns the
    keyboard. Both directions are asserted: dropped while focused, delivered
    once focus returns, so the guard cannot pass by breaking paste outright.
    """
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(bracketed_paste("PASTELEAK"))
    pump(1.5)
    assert not any("PASTELEAK" in r for r in screen.display), (
        "a paste reached the shell while a panel had focus:\n"
        + "\n".join(screen.display)
    )
    assert child.isalive(), "the client died handling a paste with a panel focused"

    # Leaving the sidebar restores paste -- the guard drops it, it does not
    # break bracketed paste.
    child.send(ALT_L)
    pump(1.0)
    assert not focused_rows(screen), "Alt+l did not leave the sidebar"
    child.send(bracketed_paste("PASTEOK"))
    pump(1.5)
    assert any("PASTEOK" in r[SIDEBAR_W:] for r in screen.display), (
        "paste stayed broken after focus returned to the content:\n"
        + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_paste_does_not_leak_past_a_focused_sidebar")


def test_a_mixed_chain_does_not_earn_the_directional_exemption():
    """`Alt-5 = "PaneFocusLeft; SetMaster"` must NOT pass through a focused panel.

    The exemption passes the WHOLE key to `ExecuteChain`, so letting a chain
    through because it merely *contains* a `PaneFocus*` would forward its other
    commands to the server while a panel has the keyboard. A mixed chain goes to
    the plugin like any other key.
    """
    env = make_env(CFG_EXEMPTIONS)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(b"\x1b5")
    pump(1.5)

    log = server_log()
    assert "SetMaster" not in log, (
        "a mixed chain leaked its non-directional command to the server:\n"
        + "\n".join(ln for ln in log.splitlines() if "msg=Command" in ln)
    )
    assert not pane_focus_cmds(log), (
        f"the chain's directional command leaked too: {pane_focus_cmds(log)}"
    )
    # The key went to the PLUGIN, which is why nothing reached the server. The
    # placeholder only reacts visibly to j/k, so the discriminator is the client
    # log: the sidebar routing `continue`s before `handle_key`, so a key the
    # exemption wrongly passed through would be logged there.
    assert "handle_key code=Char('5')" not in client_log(), (
        "the mixed chain passed through to the input handler instead of the "
        "plugin; it only did not reach the server by luck:\n"
        + "\n".join(ln for ln in client_log().splitlines() if "Char('5')" in ln)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_mixed_chain_does_not_earn_the_directional_exemption")


def test_a_group_prefix_shortcut_still_opens_command_mode():
    """`Alt-6 = "@p"` is a second entrance to command mode, not a plugin key.

    `is_prefix_key` matching only the leader would let the plugin eat it, so a
    user who reaches the Pane group by shortcut rather than by `Ctrl-a p` could
    not do so from inside a sidebar.
    """
    env = make_env(CFG_EXEMPTIONS)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(b"\x1b6")
    pump(1.5)
    assert any("focus left" in r for r in screen.display), (
        "a group-prefix shortcut did not open command mode from a focused "
        "panel:\n" + "\n".join(screen.display)
    )
    child.send(b"\x1b")
    pump(1.0)
    assert child.isalive(), "the client died leaving the group"

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_group_prefix_shortcut_still_opens_command_mode")


def state_file() -> str:
    return f"{RUNDIR}/state/remux/sidebar.json"


def test_a_toggled_sidebar_survives_a_client_restart():
    """Persistence, end to end: toggle, quit, come back, still toggled.

    Driven BOTH ways round. A one-directional test (hide, restart, still
    hidden) passes against a `save` that only ever writes "hidden" and against
    an `apply` that only ever hides, so the second leg toggles back ON and
    restarts again.

    Each leg asserts twice -- the panels are gone/back AND the server's frame
    starts at column 0 / does not -- because marker absence alone is also what a
    dead client looks like. `child.isalive()` covers the rest.
    """
    env = make_env(CFG_ACTIONS)
    child, screen, pump = spawn(env)
    pump(1.5)
    assert len(markers(screen)) == 2, f"expected two panels: {markers(screen)}"

    # -- Hide it, and check the state actually reached disk. --
    child.send(b"\x1b1")
    pump(1.5)
    assert not markers(screen), f"the sidebar did not hide: {markers(screen)}"
    assert os.path.exists(state_file()), (
        "toggling a sidebar wrote no state file:\n" + client_log()[-2000:]
    )
    body = open(state_file()).read()
    assert '"visible": false' in body, f"the hidden sidebar was not saved: {body}"

    # -- Restart the client against the SAME XDG_STATE_HOME. --
    child.close(force=True)
    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), "the client died on restart:\n" + client_log()[-2000:]
    assert not markers(screen), (
        "the hidden sidebar came back after a restart:\n"
        + "\n".join(screen.display[:4])
    )
    assert pane_frame_column(screen) == 0, (
        "the restarted client did not size the content rect for a hidden "
        "sidebar:\n" + "\n".join(screen.display[:3])
    )

    # -- Show it again, restart again: the state is not write-once. --
    child.send(b"\x1b1")
    pump(1.5)
    assert len(markers(screen)) == 2, f"the sidebar did not come back: {markers(screen)}"
    body = open(state_file()).read()
    assert '"visible": true' in body, f"the reopened sidebar was not saved: {body}"

    child.close(force=True)
    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), "the client died on the second restart"
    assert len(markers(screen)) == 2, (
        "the restored sidebar did not survive the second restart:\n"
        + "\n".join(screen.display[:4])
    )
    assert pane_frame_column(screen, SIDEBAR_W) == SIDEBAR_W, (
        "the content rect did not shrink for the restored sidebar:\n"
        + "\n".join(screen.display[:3])
    )

    # The keyboard still works after all that.
    child.send(b"echo nav9\r")
    pump(1.2)
    assert any("nav9" in r[SIDEBAR_W:] for r in screen.display), (
        "the keyboard was stranded after restoring:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_toggled_sidebar_survives_a_client_restart")


def test_a_persisted_size_and_weight_are_restored():
    """A hand-written state file stands in for the resize/weight commands.

    Nothing mutates `size` or `weight` at runtime yet, so this is the only way
    to drive those two fields end to end: 30 columns in the config, 18 in the
    state, and a 3:1 weight split that moves the second panel's marker row.
    """
    env = make_env(CFG_ACTIONS)
    os.makedirs(f"{RUNDIR}/state/remux", exist_ok=True)
    with open(state_file(), "w") as fh:
        fh.write(
            '{"bars":[{"edge":"left","visible":true,"size":18,'
            '"weights":[3,1]}]}'
        )

    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), "the client died reading state:\n" + client_log()[-2000:]

    # 18 columns wide, not 30: the panels paint inside the first 18 and the
    # server's frame begins there.
    assert markers(screen, 0, 18), (
        "the panels did not paint in an 18-column sidebar:\n"
        + "\n".join(screen.display[:4])
    )
    assert screen.display[0][18] == "\u256d", (
        "the content rect was not sized for the PERSISTED width:\n"
        + "\n".join(screen.display[:3])
    )

    # 3:1 of 30 rows puts the second panel's marker well below the halfway row
    # a 1:1 config split would give it.
    rows_seen = [y for (y, _) in markers(screen, 0, 18)]
    assert len(rows_seen) == 2, f"expected two panels: {markers(screen, 0, 18)}"
    # A 1:1 config split puts it on row 16 of 30; 3:1 puts it on row 23. The
    # threshold has to sit between the two or the assertion is blind to whether
    # `weights` was applied at all.
    assert rows_seen[1] >= 20, (
        f"the persisted 3:1 weight split was not applied: {rows_seen}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_persisted_size_and_weight_are_restored")


def test_a_corrupt_state_file_falls_back_to_the_config():
    """Never fail a client start over persisted chrome state."""
    env = make_env(CFG_ACTIONS)
    os.makedirs(f"{RUNDIR}/state/remux", exist_ok=True)
    with open(state_file(), "w") as fh:
        fh.write("{ not json")

    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), (
        "a corrupt state file killed the client:\n" + client_log()[-2000:]
    )
    assert len(markers(screen)) == 2, (
        "the config defaults were not used after a corrupt state file:\n"
        + "\n".join(screen.display[:4])
    )
    assert "sidebar state" in client_log(), (
        "the corrupt state file was discarded silently"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_corrupt_state_file_falls_back_to_the_config")


def test_state_for_an_edge_the_config_no_longer_declares_is_ignored():
    """The user moved their sidebar to the other edge after state was written.

    The config declares a CLOSED right sidebar; the state names the left one.
    Neither may open: nothing on screen is a marker. Run against a config with
    no sidebar at all this would be blind -- with an empty `chrome.sidebars`
    even a completely broken `apply` has nothing to write to.
    """
    env = make_env(CFG_RIGHT_HIDDEN)
    os.makedirs(f"{RUNDIR}/state/remux", exist_ok=True)
    with open(state_file(), "w") as fh:
        fh.write('{"bars":[{"edge":"left","visible":true,"size":30,"weights":[1]}]}')

    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), (
        "state for an undeclared edge killed the client:\n" + client_log()[-2000:]
    )
    assert not markers(screen, 0, COLS), (
        "state for one edge opened a sidebar on another:\n"
        + "\n".join(screen.display[:4])
    )
    child.send(b"echo nav10\r")
    pump(1.2)
    assert any("nav10" in r for r in screen.display), (
        "the shell was unreachable:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_state_for_an_edge_the_config_no_longer_declares_is_ignored")


def test_with_no_sidebar_a_toggle_writes_no_state_file():
    """The regression gate: no `[[sidebar]]`, no new file on disk."""
    env = make_env(CFG_ACTIONS_NO_SIDEBAR)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(b"\x1b1")
    pump(1.5)
    assert child.isalive(), "the client died toggling a sidebar that is not there"
    assert not os.path.exists(state_file()), (
        "a no-op toggle with no sidebar configured wrote a state file"
    )
    child.send(b"echo nav11\r")
    pump(1.2)
    assert any("nav11" in r for r in screen.display), (
        "the shell was unreachable:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_with_no_sidebar_a_toggle_writes_no_state_file")


def seam(screen):
    """The column the server's frame starts at: its pane border's top-left.

    The sidebar's own width is not readable from the panel content (the
    placeholder pads), so the seam is what proves a resize actually moved the
    boundary rather than only the stored number.
    """
    row = screen.display[0]
    # The sidebar is framed too, in the same style, and its box's top-left
    # corner sits at column 0. The server's frame is the next corner along, so
    # the search starts at column 1 whenever a panel is on screen.
    start = 1 if markers(screen) else 0
    at = row.find("\u256d", start)
    return at if at >= 0 else None


def resize_cols(log: str):
    """Every content width the SERVER was resized to, in order."""
    return [int(c) for c in re.findall(r"resize cols=(\d+) rows=", log)]


def test_the_resize_chord_moves_a_focused_sidebars_edge():
    """`Prefix p R l` with the left sidebar focused grows the SIDEBAR.

    The seam is asserted on screen and the width is asserted in the server's
    log, because those are two different bugs: a stored size that never reached
    `Resize` leaves the server rendering at the old width behind a repainted
    sidebar.

    The Resize group is STICKY, so the second `l` repeats without the chord.
    """
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)
    assert seam(screen) == SIDEBAR_W, f"the frame did not start at the seam: {seam(screen)}"

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(b"\x01pRl")
    pump(1.5)
    child.send(b"l")
    pump(1.5)
    child.send(b"\x1b")
    pump(1.0)

    assert seam(screen) == SIDEBAR_W + 10, (
        f"two sticky `l` presses did not grow the sidebar by 10: seam={seam(screen)}\n"
        + "\n".join(screen.display[:3])
    )
    widths = resize_cols(server_log())
    assert COLS - SIDEBAR_W - 5 in widths and COLS - SIDEBAR_W - 10 in widths, (
        f"the server was never resized to the new content widths: {widths}"
    )
    # Nothing leaked: the server must not have been asked to resize a PANE.
    assert "msg=Command(Resize" not in server_log(), (
        "a resize inside a sidebar leaked to the server as a pane resize"
    )

    # The keyboard is still the PANEL's -- the Escape above left the sticky
    # group, not the sidebar -- so leave it the normal way and check the shell
    # is reachable at the sidebar's new width.
    child.send(ALT_L)
    pump(1.0)
    assert not focused_rows(screen), f"Alt+l did not leave the panel: {markers(screen)}"
    child.send(b"echo nav12\r")
    pump(1.2)
    assert any("nav12" in r[SIDEBAR_W + 10 :] for r in screen.display), (
        "the keyboard was stranded after resizing:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_resize_chord_moves_a_focused_sidebars_edge")


def test_the_resize_chord_reweights_along_the_stack():
    """`Prefix p R j` moves the focused PANEL's weight, not the sidebar's size.

    Down grows the focused panel downward -- the same thing it does to a focused
    pane on the server -- so the second panel's marker moves DOWN the screen and
    the seam does not move at all.
    """
    env = make_env(CFG_LEFT_TWO_PANELS)
    child, screen, pump = spawn(env)
    pump(1.5)
    before = [y for (y, _) in markers(screen)]
    assert len(before) == 2, f"expected two stacked panels: {markers(screen)}"

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen) == [before[0]], "the top panel never took focus"

    child.send(b"\x01pRj")
    pump(1.5)
    child.send(b"\x1b")
    pump(1.0)

    after = [y for (y, _) in markers(screen)]
    assert len(after) == 2, f"reweighting dropped a panel: {markers(screen)}"
    assert after[1] > before[1], (
        f"the second panel did not move down: {before} -> {after}"
    )
    assert seam(screen) == SIDEBAR_W, (
        f"the stack axis moved the sidebar's SIZE: seam={seam(screen)}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_the_resize_chord_reweights_along_the_stack")


def test_an_alt_bound_resize_reaches_the_sidebar_not_the_plugin():
    """A pure `Resize*` shortcut earns the same exemption `PaneFocus*` does.

    Without it, a user whose only resize binding is a Normal-mode shortcut
    cannot resize the sidebar from inside it -- the plugin eats the key. The
    mixed chain in the same config must NOT earn it: its `SetMaster` half would
    reach the server while a panel has the keyboard.
    """
    env = make_env(CFG_ALT_RESIZE)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    child.send(b"\x1b7")
    pump(1.5)
    assert seam(screen) == SIDEBAR_W + 5, (
        f"an Alt-bound resize never reached the sidebar: seam={seam(screen)}\n"
        + "\n".join(screen.display[:3])
    )
    assert "msg=Command(Resize" not in server_log(), (
        "the Alt-bound resize leaked to the server"
    )

    # The mixed chain goes to the plugin: no resize, and no `SetMaster`.
    at = seam(screen)
    child.send(b"\x1b8")
    pump(1.5)
    assert seam(screen) == at, (
        f"a mixed chain earned the resize exemption: {at} -> {seam(screen)}"
    )
    assert "SetMaster" not in server_log(), (
        "a mixed chain forwarded its other half to the server from inside a panel"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_an_alt_bound_resize_reaches_the_sidebar_not_the_plugin")


def test_a_resize_with_focus_on_the_content_still_reaches_the_server():
    """The regression gate: unfocused sidebar, resize behaves exactly as before."""
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)
    assert not focused_rows(screen), "the sidebar must start unfocused"

    child.send(b"\x01pRl")
    pump(1.5)
    child.send(b"\x1b")
    pump(1.0)

    assert "msg=Command(ResizeRight(5))" in server_log(), (
        "a resize with focus on the content did not reach the server:\n"
        + server_log()[-1500:]
    )
    assert seam(screen) == SIDEBAR_W, (
        f"a content resize moved the SIDEBAR: seam={seam(screen)}"
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_resize_with_focus_on_the_content_still_reaches_the_server")


def test_a_resized_sidebar_survives_a_client_restart():
    """The size the user dragged to is remembered, like the visibility is."""
    env = make_env(CFG_LEFT)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    child.send(b"\x01pRl")
    pump(1.5)
    child.send(b"\x1b")
    pump(1.0)
    grown = seam(screen)
    assert grown == SIDEBAR_W + 5, f"the sidebar did not grow: seam={grown}"
    assert os.path.exists(state_file()), (
        "resizing a sidebar wrote no state file:\n" + client_log()[-2000:]
    )
    body = open(state_file()).read()
    assert f'"size": {SIDEBAR_W + 5}' in body, f"the new size was not saved: {body}"

    child.close(force=True)
    child, screen, pump = spawn(env)
    pump(2.0)
    assert child.isalive(), "the client died on restart:\n" + client_log()[-2000:]
    assert seam(screen) == grown, (
        f"the resized sidebar came back at its config width: seam={seam(screen)}\n"
        + "\n".join(screen.display[:3])
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_resized_sidebar_survives_a_client_restart")


def test_a_directional_key_in_a_sticky_group_is_intercepted():
    """Task 7's invariant holds in the sticky-group arm too.

    Sticky-group leaves reach `ExecuteAndShowWhichKey`, never `Execute`, so the
    two interception sites Task 7 installed do not cover them. A config that
    merges a `PaneFocus*` into the built-in Resize group is a normal thing to
    write, and without this the direction would reach the server from inside a
    focused panel.

    Both halves are driven: a direction with nowhere to go is SWALLOWED, and one
    pointing at the content LEAVES the sidebar. Swallow-only would pass against
    an interception that consumes everything and routes nothing.
    """
    env = make_env(CFG_STICKY_FOCUS)
    child, screen, pump = spawn(env)
    pump(1.5)

    child.send(ALT_H)
    pump(1.0)
    assert focused_rows(screen), "the sidebar never took focus"

    # `p R x` = PaneFocusLeft, from inside the LEFT sidebar: nowhere to go.
    child.send(b"\x01pRx")
    pump(1.5)
    leaked = pane_focus_cmds(server_log())
    assert not leaked, (
        f"a directional key in a sticky group leaked to the server as {leaked}"
    )
    assert focused_rows(screen), (
        f"the swallowed direction moved focus out of the sidebar: {markers(screen)}"
    )

    # `p R y` = PaneFocusRight: toward the content, so it hands the keyboard
    # back. Still sticky, so no fresh chord is needed.
    child.send(b"y")
    pump(1.5)
    assert not focused_rows(screen), (
        f"the direction toward the content did not leave the sidebar: {markers(screen)}"
    )
    assert not pane_focus_cmds(server_log()), (
        "leaving the sidebar forwarded the direction to the server as well"
    )

    child.send(b"\x1b")
    pump(1.0)
    child.send(b"echo nav13\r")
    pump(1.2)
    assert any("nav13" in r[SIDEBAR_W:] for r in screen.display), (
        "the keyboard was stranded:\n" + "\n".join(screen.display)
    )

    teardown(child, env)
    check_no_panic()
    print("PASS test_a_directional_key_in_a_sticky_group_is_intercepted")


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
    test_a_paste_does_not_leak_past_a_focused_sidebar()
    test_a_mixed_chain_does_not_earn_the_directional_exemption()
    test_a_group_prefix_shortcut_still_opens_command_mode()
    test_a_directional_key_in_a_sticky_group_is_intercepted()
    test_the_resize_chord_moves_a_focused_sidebars_edge()
    test_the_resize_chord_reweights_along_the_stack()
    test_an_alt_bound_resize_reaches_the_sidebar_not_the_plugin()
    test_a_resize_with_focus_on_the_content_still_reaches_the_server()
    test_a_resized_sidebar_survives_a_client_restart()
    test_a_toggled_sidebar_survives_a_client_restart()
    test_a_persisted_size_and_weight_are_restored()
    test_a_corrupt_state_file_falls_back_to_the_config()
    test_state_for_an_edge_the_config_no_longer_declares_is_ignored()
    test_with_no_sidebar_a_toggle_writes_no_state_file()
    test_with_no_sidebar_every_directional_key_is_unchanged()
    print("ALL PASS")
