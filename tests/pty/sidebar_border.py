#!/usr/bin/env python3
"""Sidebar frames: the sidebars wear the session's border style.

A sidebar is CLIENT-side chrome. The server never draws it, so only a real PTY
sees the composited result -- a frame-level harness sees the content rect alone
and would pass on a sidebar that never drew a single glyph.

EVERY test here runs with a `[[sidebar]]` configured. With none, `panel_rects`
is empty, nothing is painted, and every assertion below would be vacuous.

What is covered:
  * the frame is drawn, in BOTH border styles (a box for zellij, a seam for tmux)
  * `ToggleStyle` reframes the sidebar in the same keystroke it reframes the panes
  * a rule separates stacked panels
  * a focused sidebar's frame takes the active colour; an unfocused one does not
  * a sidebar too small to frame degrades to the unframed rendering
  * the panel's usable INTERIOR really shrank -- asserted on the plugin's own
    text being clipped by the frame, not merely on the frame glyphs existing
  * a click on the frame is swallowed and never reaches the server

Run: python3 tests/pty/sidebar_border.py
"""
import os
import re
import shutil
import subprocess
import time

import pexpect
import pyte

BIN = os.path.abspath(os.environ.get("REMUX_BIN", "target/debug/remux"))
RUNDIR = "/tmp/rmx-sbb"
COLS, ROWS = 100, 30
SIDEBAR_W = 30

# Distinctive theme colours so pyte reports an unambiguous hex for each.
FRAME_FG = "585b70"
FRAME_ACTIVE_FG = "89b4fa"

TL, TR, BL, BR = "╭", "╮", "╰", "╯"
HORZ, VERT = "─", "│"
TEE_L, TEE_R = "├", "┤"
BOX = set(TL + TR + BL + BR + HORZ + VERT + TEE_L + TEE_R + "┬┴┼")

FAILURES = []


def cfg(style="zellij_style", size=SIDEBAR_W, panels=1):
    panel_block = "".join(
        """
  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""
        for _ in range(panels)
    )
    return f"""
[appearance]
border_style = "{style}"

[appearance.theme]
frame_fg = "#{FRAME_FG}"
frame_active_fg = "#{FRAME_ACTIVE_FG}"

[[sidebar]]
edge = "left"
size = {size}
visible = true
{panel_block}"""


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
    """A panic in either log fails the test.

    `install_panic_logger()` routes `std::panic` through the `log` crate, so
    the hook's `panicked at ...` really does land in these files. Probed by
    hand: with a `panic!("probe")` in `Chrome::paint`, this fired.
    """
    for name in ("client.log", "server.log"):
        log = f"{RUNDIR}/state/remux/{name}"
        if os.path.exists(log):
            body = open(log, errors="replace").read()
            assert "panicked" not in body, f"{name} panicked:\n{body[-2000:]}"


def client_log() -> str:
    path = f"{RUNDIR}/state/remux/client.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def write_config(body: str):
    """Rewrite the live config and give the watcher time to debounce + apply."""
    with open(f"{RUNDIR}/config/remux/config.toml", "w") as fh:
        fh.write(body)
    time.sleep(0.2)


def server_log() -> str:
    path = f"{RUNDIR}/state/remux/server.log"
    return open(path, errors="replace").read() if os.path.exists(path) else ""


def clicks(log: str):
    """Every `server: MouseClick` the server handled, as (x, y, release)."""
    return [
        (int(m.group(1)), int(m.group(2)), m.group(3) == "true")
        for m in re.finditer(
            r"server: MouseClick client_id=\d+ x=(\d+) y=(\d+) release=(\w+)", log
        )
    ]


def sgr_press(col, row):
    return f"\x1b[<0;{col};{row}M".encode()


def sgr_release(col, row):
    return f"\x1b[<0;{col};{row}m".encode()


def marker_rows(screen):
    """(row, "focused"/"idle") for every placeholder marker in the sidebar."""
    out = []
    for y, row in enumerate(screen.display):
        band = row[:SIDEBAR_W]
        if "focused" in band:
            out.append((y, "focused"))
        elif "idle" in band:
            out.append((y, "idle"))
    return out


def fail(name, msg, screen=None):
    FAILURES.append(f"{name}: {msg}")
    print(f"  FAIL {name}\n       {msg}")
    if screen is not None:
        for i, r in enumerate(screen.display[:6]):
            print(f"       {i:2} |{r.rstrip()}")


def ok(name):
    print(f"PASS {name}")


# ---------------------------------------------------------------------------


def test_zellij_style_draws_a_box_around_the_sidebar():
    name = "test_zellij_style_draws_a_box_around_the_sidebar"
    env = make_env(cfg("zellij_style"))
    child, screen, pump = spawn(env)
    pump(1.5)
    rows = screen.display
    w = SIDEBAR_W

    bad = []
    if rows[0][0] != TL or rows[0][w - 1] != TR:
        bad.append(f"top corners: {rows[0][:w]!r}")
    if rows[ROWS - 1][0] != BL or rows[ROWS - 1][w - 1] != BR:
        bad.append(f"bottom corners: {rows[ROWS - 1][:w]!r}")
    if rows[0][1 : w - 1] != HORZ * (w - 2):
        bad.append(f"top edge: {rows[0][:w]!r}")
    if rows[ROWS - 1][1 : w - 1] != HORZ * (w - 2):
        bad.append(f"bottom edge: {rows[ROWS - 1][:w]!r}")
    sides = [y for y in range(1, ROWS - 1) if rows[y][0] != VERT or rows[y][w - 1] != VERT]
    if sides:
        bad.append(f"rows missing a side edge: {sides[:5]}")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)

    teardown(child, env)
    check_no_panic()


def test_the_panel_interior_shrank_by_the_frame():
    """The load-bearing one: assert on the CONTENT inside the frame.

    A 10-column sidebar has an 8-column interior under the zellij box, so the
    placeholder's title is clipped to 8 characters and the frame's right edge
    follows immediately. Had the panel still been handed the full bar, the same
    row would read `Placehold` from column 0 -- so this fails on a frame that is
    merely painted over a panel that never shrank.
    """
    name = "test_the_panel_interior_shrank_by_the_frame"
    env = make_env(cfg("zellij_style", size=10))
    child, screen, pump = spawn(env)
    pump(1.5)
    got = screen.display[1][:10]
    want = VERT + "Placehol" + VERT
    if got != want:
        fail(name, f"panel row is {got!r}, expected {want!r}", screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_tmux_style_draws_a_seam_and_no_box():
    """tmux panes have no box, so a tmux sidebar gets only the content seam."""
    name = "test_tmux_style_draws_a_seam_and_no_box"
    env = make_env(cfg("tmux_style", size=10))
    child, screen, pump = spawn(env)
    pump(1.5)
    rows = screen.display

    bad = []
    seam = [y for y in range(ROWS) if rows[y][9] != VERT]
    if seam:
        bad.append(f"rows missing the seam divider at column 9: {seam[:5]}")
    boxed = [y for y in range(ROWS) if rows[y][0] in BOX]
    if boxed:
        bad.append(f"a left edge was drawn on rows {boxed[:5]} -- tmux has no box")
    # The interior is 9 columns, one more than zellij's, and it starts at
    # column 0: the seam is the only cell the frame took.
    if rows[0][:10] != "Placehold" + VERT:
        bad.append(f"panel row is {rows[0][:10]!r}, expected 'Placehold' + the seam")
    if str(screen.buffer[5][9].fg) != FRAME_FG:
        bad.append(
            f"the seam is {screen.buffer[5][9].fg}, not frame_fg -- tmux pane "
            f"dividers are always frame_fg and the sidebar must match them"
        )
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_toggle_style_reframes_the_sidebar_with_the_panes():
    """One keystroke must move both, or the sidebar contradicts the panes."""
    name = "test_toggle_style_reframes_the_sidebar_with_the_panes"
    # size=10 so the panel's own content proves which frame is in force: the
    # zellij box leaves an 8-column interior ("Placehol"), the tmux seam a
    # 9-column one ("Placehold"). Asserting only on corner/seam GLYPHS would
    # pass on a build that flipped the frame and kept the old interior -- the
    # frame-right-content-wrong shape this branch keeps producing.
    env = make_env(cfg("zellij_style", size=10))
    child, screen, pump = spawn(env)
    pump(1.5)
    rows = screen.display
    bad = []
    if rows[0][0] != TL:
        bad.append(f"the sidebar did not start framed: {rows[0][:10]!r}")
    if rows[0][10] != TL:
        bad.append(f"the pane did not start framed: {rows[0][10:][:12]!r}")
    # The zellij interior: 8 columns, then the box's right edge.
    if rows[1][:10] != VERT + "Placehol" + VERT:
        bad.append(f"zellij interior wrong before the toggle: {rows[1][:10]!r}")

    child.send(b"\x01")
    time.sleep(0.2)
    child.send(b"g")  # ToggleStyle
    pump(1.5)
    rows = screen.display
    # tmux style: neither the sidebar nor the pane has a box any more, and the
    # sidebar's seam is the only divider left on the left of the screen.
    if rows[0][0] in BOX:
        bad.append(f"the sidebar kept its box after the toggle: {rows[0][:10]!r}")
    if rows[0][9] != VERT:
        bad.append(f"the sidebar has no tmux seam: {rows[0][:10]!r}")
    if rows[0][10] == TL:
        bad.append("the panes kept their box after the toggle")
    # ...and the INTERIOR really moved with the frame: 9 columns now, starting
    # at column 0. This is the assertion a frame-only test would miss.
    if rows[0][:10] != "Placehold" + VERT:
        bad.append(
            f"the panel interior did not follow the toggle: {rows[0][:10]!r}, "
            f"expected 'Placehold' + the seam"
        )

    # ... and back again, in one keystroke.
    child.send(b"\x01")
    time.sleep(0.2)
    child.send(b"g")
    pump(1.5)
    rows = screen.display
    if rows[0][0] != TL:
        bad.append(f"the sidebar did not come back framed: {rows[0][:10]!r}")
    if rows[0][10] != TL:
        bad.append("the panes did not come back framed")
    if rows[1][:10] != VERT + "Placehol" + VERT:
        bad.append(f"the interior did not come back with it: {rows[1][:10]!r}")

    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_a_rule_separates_stacked_panels():
    name = "test_a_rule_separates_stacked_panels"
    env = make_env(cfg("zellij_style", panels=2))
    child, screen, pump = spawn(env)
    pump(1.5)
    rows = screen.display
    w = SIDEBAR_W

    bad = []
    markers = marker_rows(screen)
    if len(markers) != 2:
        bad.append(f"expected two stacked panels, saw {markers}")
    rules = [
        y
        for y in range(ROWS)
        if rows[y][0] == TEE_L
        and rows[y][w - 1] == TEE_R
        and rows[y][1 : w - 1] == HORZ * (w - 2)
    ]
    if len(rules) != 1:
        bad.append(f"expected exactly one rule row, found {rules}")
    elif len(markers) == 2 and not (markers[0][0] < rules[0] < markers[1][0]):
        bad.append(f"the rule at row {rules[0]} is not between the panels {markers}")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_the_focused_sidebars_frame_uses_the_active_colour():
    name = "test_the_focused_sidebars_frame_uses_the_active_colour"
    env = make_env(cfg("zellij_style"))
    child, screen, pump = spawn(env)
    pump(1.5)
    bad = []
    if str(screen.buffer[0][0].fg) != FRAME_FG:
        bad.append(f"an unfocused sidebar's frame is {screen.buffer[0][0].fg}")

    child.send(b"\x1bh")  # Alt+h -- enter the left sidebar
    pump(1.2)
    if not [m for m in marker_rows(screen) if m[1] == "focused"]:
        bad.append("Alt+h never focused the panel; the colour check is vacuous")
    if str(screen.buffer[0][0].fg) != FRAME_ACTIVE_FG:
        bad.append(f"a focused sidebar's frame is {screen.buffer[0][0].fg}")

    child.send(b"\x1bl")  # Alt+l -- back to the content
    pump(1.2)
    if str(screen.buffer[0][0].fg) != FRAME_FG:
        bad.append(f"the frame stayed active after leaving: {screen.buffer[0][0].fg}")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_a_sidebar_too_small_to_frame_degrades_to_unframed():
    """`fits_zellij_border` is 3x3; a 2-column bar renders as it always did."""
    name = "test_a_sidebar_too_small_to_frame_degrades_to_unframed"
    env = make_env(cfg("zellij_style", size=2))
    child, screen, pump = spawn(env)
    pump(1.5)
    rows = screen.display
    bad = []
    drawn = [y for y in range(ROWS) if rows[y][0] in BOX or rows[y][1] in BOX]
    if drawn:
        bad.append(f"a broken box was drawn on rows {drawn[:5]}: {rows[0][:4]!r}")
    if rows[0][:2] != "Pl":
        bad.append(f"the panel did not render unframed: {rows[0][:4]!r}")
    if rows[0][2] != TL:
        bad.append(f"the content rect did not start at column 2: {rows[0][:6]!r}")
    if not child.isalive():
        bad.append("the client died on an unframeable sidebar")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_a_click_on_the_frame_never_reaches_the_server():
    """The frame belongs to no panel -- and must not fall through either.

    `panel_rects` returns interiors, so a press on the border hits no panel. The
    control click into the content comes first: without it, "no MouseClick after
    this point" would pass on a build where the mouse was never wired up at all.
    """
    name = "test_a_click_on_the_frame_never_reaches_the_server"
    env = make_env(cfg("zellij_style"))
    child, screen, pump = spawn(env)
    pump(1.5)
    bad = []

    # Control: a click in the CONTENT does reach the server.
    child.send(sgr_press(60, 6))
    child.send(sgr_release(60, 6))
    pump(1.2)
    control = clicks(server_log())
    if not control:
        bad.append("no click ever reached the server; the assertion below is vacuous")

    before_marker = marker_rows(screen)
    # The sidebar's RIGHT border, 1-based column SIDEBAR_W: inside the bar,
    # inside no panel.
    child.send(sgr_press(SIDEBAR_W, 6))
    child.send(sgr_release(SIDEBAR_W, 6))
    pump(1.2)
    after = clicks(server_log())
    if after != control:
        bad.append(f"a click on the frame reached the server: {after[len(control):]}")
    if marker_rows(screen) != before_marker:
        bad.append(
            f"a click on the frame reached the plugin: "
            f"{before_marker} -> {marker_rows(screen)}"
        )
    if not child.isalive():
        bad.append("the client died on a frame click")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_a_config_reload_keeps_the_toggled_style():
    """A hot-reload rebuilds the `Chrome`; it must not un-toggle the frame.

    `Chrome::from_config` starts from the config default, so a reload that did
    not carry the live style across would silently put the sidebar back into
    `appearance.border_style` while the panes stayed toggled -- the sidebar and
    the panes disagreeing, which is the exact inconsistency this work removes.

    The edit is to `size`, not to the style: that makes the reload observable on
    screen independently of the log, so "still toggled" cannot pass because
    nothing reloaded.
    """
    name = "test_a_config_reload_keeps_the_toggled_style"
    env = make_env(cfg("zellij_style"))
    child, screen, pump = spawn(env)
    pump(1.5)
    bad = []
    if screen.display[0][0] != TL:
        bad.append("the sidebar did not start framed in the config's zellij style")

    child.send(b"\x01")
    time.sleep(0.2)
    child.send(b"g")  # ToggleStyle -> tmux
    pump(1.5)
    if screen.display[0][SIDEBAR_W - 1] != VERT or screen.display[0][0] in BOX:
        bad.append(f"the toggle did not reach the sidebar: {screen.display[0][:SIDEBAR_W]!r}")
    # The interior moved with the frame, not just the glyphs: a 30-column tmux
    # sidebar leaves 29 columns, so the seam sits immediately after the panel's
    # own content band rather than one column further in.
    if screen.display[0][:SIDEBAR_W].rstrip(" ")[-1] != VERT:
        bad.append(
            f"the tmux seam is not the last cell of the bar: "
            f"{screen.display[0][:SIDEBAR_W]!r}"
        )

    narrower = 26
    baseline = client_log().count("client: config reloaded")
    write_config(cfg("zellij_style", size=narrower))
    pump(3.0)
    reloads = client_log().count("client: config reloaded") - baseline
    if reloads < 1:
        bad.append("the config edit never reloaded; the assertion below is vacuous")
    rows = screen.display
    # The reload DID take effect...
    if rows[0][narrower - 1] != VERT:
        bad.append(
            f"the reload did not apply the new size: {rows[0][:SIDEBAR_W]!r}"
        )
    # ...and the INTERIOR is the tmux one at the NEW size: 25 columns of panel
    # then the seam, with no box column at 0. Glyph-only assertions would pass
    # on a build that kept the zellij interior behind a tmux-looking seam.
    if rows[0][:narrower] != "Placeholder".ljust(narrower - 1) + VERT:
        bad.append(
            f"the panel interior is not the tmux one at the new size: "
            f"{rows[0][:narrower]!r}"
        )
    # ...and the sidebar is still in the TOGGLED style, not the config's.
    if rows[0][0] in BOX:
        bad.append(
            f"the reload reverted the sidebar to the config's zellij style: "
            f"{rows[0][:SIDEBAR_W]!r}"
        )
    # ...and the panes agree with it.
    if rows[0][narrower] == TL:
        bad.append("the panes are framed while the sidebar is not")
    if not child.isalive():
        bad.append("the client died across the reload")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


def test_a_reattach_resyncs_the_style_the_server_is_drawing():
    """The style is per-SESSION server state; a fresh client must learn it.

    `ToggleStyle` flips `Session::border_style` on the server AND the client's
    own copy. The client's copy is seeded once from `appearance.border_style`,
    so a client that attaches AFTER a toggle used to come up in the config's
    style while the server kept compositing the panes in the toggled one: panes
    tmux, sidebar zellij, and no way to resync short of toggling twice.

    Two clients rather than a detach/reattach of one, because the second client
    is the honest test: it has never seen the toggle, so nothing but the
    server's answer can tell it what style to frame in.
    """
    name = "test_a_reattach_resyncs_the_style_the_server_is_drawing"
    env = make_env(cfg("zellij_style", size=10))
    first, screen, pump = spawn(env)
    pump(1.5)
    bad = []
    if screen.display[0][0] != TL:
        bad.append("the first client did not start framed in zellij style")

    first.send(b"\x01")
    time.sleep(0.2)
    first.send(b"g")  # ToggleStyle -> tmux, on the server's session
    pump(1.5)
    if screen.display[0][0] in BOX:
        bad.append(f"the toggle did not take: {screen.display[0][:10]!r}")
    first.close(force=True)
    time.sleep(0.5)

    # A brand-new client attaches to the same (toggled) session.
    second, screen2, pump2 = spawn(env)
    pump2(2.0)
    rows = screen2.display
    # The PANES are tmux (the server's session state survived).
    if rows[0][10] == TL:
        bad.append(f"the panes are not in the toggled style: {rows[0][:22]!r}")
    # ...and the SIDEBAR agrees, rather than reverting to the config's zellij.
    if rows[0][0] in BOX:
        bad.append(
            f"the reattached client framed the sidebar in the config's style "
            f"while the panes are in the session's: {rows[0][:12]!r}"
        )
    # The interior moved with it: tmux leaves 9 columns then the seam.
    if rows[0][:10] != "Placehold" + VERT:
        bad.append(
            f"the panel interior is not the tmux one after the reattach: "
            f"{rows[0][:10]!r}"
        )
    if not second.isalive():
        bad.append("the second client died")
    if bad:
        fail(name, "; ".join(bad), screen2)
    else:
        ok(name)
    teardown(second, env)
    check_no_panic()


def test_only_one_frame_reads_as_focused():
    """The pane and the sidebar must never both look active at once.

    The server composites one frame per session and has never heard of
    sidebars, so it draws the session's focused pane with an ACTIVE border
    whatever the client is doing with its chrome; the client then lights the
    focused sidebar's frame on top of that. Each half is right on its own and
    together they are wrong -- two rings both claiming the keyboard.

    Both halves are asserted in all three states. A test that checked only the
    pane would pass on a build where NEITHER ring reads as focused, and one
    that checked only the corner would pass on a recolour shifted by a cell
    (`focused_pane_rect` is the pane's INTERIOR, so the ring is that rect grown
    by one), which is why the left edge is checked at mid-height too.
    """
    name = "test_only_one_frame_reads_as_focused"
    env = make_env(cfg("zellij_style"))
    child, screen, pump = spawn(env)
    pump(1.5)
    bad = []
    mid = ROWS // 2
    # (row, col, expected glyph, what it is) for the two rings. The pane's box
    # starts in the first column the content rect owns.
    probes = [
        (0, 0, TL, "the sidebar's top-left corner"),
        (mid, 0, VERT, "the sidebar's left edge"),
        (0, SIDEBAR_W, TL, "the pane's top-left corner"),
        (mid, SIDEBAR_W, VERT, "the pane's left edge"),
    ]

    def check(where, want_sidebar, want_pane):
        for y, x, glyph, what in probes:
            cell = screen.buffer[y][x]
            # Not vacuous: the cell must really be the border glyph, or the
            # colour below is the colour of whatever else landed there.
            if cell.data != glyph:
                bad.append(f"{where}: {what} is {cell.data!r}, not {glyph!r}")
                continue
            want = want_sidebar if x == 0 else want_pane
            if str(cell.fg) != want:
                bad.append(f"{where}: {what} is {cell.fg}, expected {want}")

    check("with focus in the content", FRAME_FG, FRAME_ACTIVE_FG)

    child.send(b"\x1bh")  # Alt+h -- into the left sidebar
    pump(1.2)
    if not [m for m in marker_rows(screen) if m[1] == "focused"]:
        bad.append("Alt+h never focused the panel; every check below is vacuous")
    check("with the sidebar focused", FRAME_ACTIVE_FG, FRAME_FG)

    child.send(b"\x1bl")  # Alt+l -- back to the content
    pump(1.2)
    check("back on the content", FRAME_FG, FRAME_ACTIVE_FG)

    if not child.isalive():
        bad.append("the client died")
    if bad:
        fail(name, "; ".join(bad), screen)
    else:
        ok(name)
    teardown(child, env)
    check_no_panic()


if __name__ == "__main__":
    test_zellij_style_draws_a_box_around_the_sidebar()
    test_the_panel_interior_shrank_by_the_frame()
    test_tmux_style_draws_a_seam_and_no_box()
    test_toggle_style_reframes_the_sidebar_with_the_panes()
    test_a_rule_separates_stacked_panels()
    test_the_focused_sidebars_frame_uses_the_active_colour()
    test_only_one_frame_reads_as_focused()
    test_a_sidebar_too_small_to_frame_degrades_to_unframed()
    test_a_click_on_the_frame_never_reaches_the_server()
    test_a_config_reload_keeps_the_toggled_style()
    test_a_reattach_resyncs_the_style_the_server_is_drawing()
    if FAILURES:
        print("FAILURES")
        for f in FAILURES:
            print(f"  - {f}")
        raise SystemExit(1)
    print("ALL PASS")
