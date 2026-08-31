#!/usr/bin/env python3
"""A config hot-reload picks up `[[sidebar]]` changes without a restart.

Reported: adding a sidebar to the config did nothing until the client was
restarted. The chrome was built once at startup and the reload arm never
touched it -- a deliberate call (plugins carry state) that the user overruled.

Four things have to move together, and each has its own assertion:

  the chrome is rebuilt   -> the panel paints
  the tree subscription
    is reconciled         -> the panel shows real ROWS, not an empty frame
  the content rect is
    re-synced AND resized -> `stty size` inside the pane drops by the sidebar's
                             width (the frame alone can be right while the pane
                             behind it is sized wrong)
  persisted runtime state
    is re-applied         -> a sidebar the user resized keeps that width across
                             an unrelated config edit

Plus the reverse direction: removing the block puts the pane back.

Every assertion runs against a real `[[sidebar]]` -- with none, `panel_rects`
is empty and none of the paint or resize paths are reachable.

Run from the repo root:  python3 tests/pty/sidebar_reload.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

COLS, ROWS = 100, 30
SIDEBAR_W = 24

# A valid config with NO sidebar. The file has to exist up front: this is the
# "I edited my config" story, not the "I created one" story, and the watcher
# reloads on Create as well as Modify either way.
CFG_NONE = """
[appearance]
border_style = "zellij_style"
"""

CFG_SESSIONS = f"""
[appearance]
border_style = "zellij_style"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "sessions"
  weight = 1
"""

# The same sidebar with an unrelated edit on top, for the persistence check.
CFG_SESSIONS_TOUCHED = CFG_SESSIONS + """
[appearance.unused_marker_section]
"""

# A placeholder panel instead of the sessions tree: it renders `focused`/`idle`
# on its second row and bumps a counter on `j`/`k`, which is how "who has the
# keyboard" is observable at all.
CFG_PLACEHOLDER = f"""
[appearance]
border_style = "zellij_style"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

CFG_PLACEHOLDER_TOUCHED = CFG_PLACEHOLDER + """
[appearance.unused_marker_section]
"""


def _stacked(w1, w2):
    return f"""
[appearance]
border_style = "zellij_style"

[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = {w1}

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = {w2}
"""


# Two evenly split panels, then the same stack reweighted 100:1. Over ROWS rows
# the second panel's share falls below the placeholder's `min_rows`, so
# `split_panels` DROPS it -- while `panels.len()` is still 2. That gap is the
# whole point: a restore clamped on the COUNT lands focus on a panel that is
# never painted.
CFG_STACKED = _stacked(1, 1)
CFG_STACKED_SECOND_DROPPED = _stacked(100, 1)

WIDER_W = 40

# The same sidebar with `size` retyped. Once `sidebar.json` exists -- and it
# exists from the first toggle or resize onward -- a startup-style overlay
# makes this edit dead, which is the "I edited my config and nothing happened"
# complaint hot-reload exists to answer.
CFG_SESSIONS_WIDER = CFG_SESSIONS.replace(
    f"size = {SIDEBAR_W}", f"size = {WIDER_W}"
)

# The same sidebar with `visible` retyped to false.
CFG_SESSIONS_HIDDEN = CFG_SESSIONS.replace("visible = true", "visible = false")

# `CFG_SESSIONS_WIDER` plus an unrelated edit: `size` is UNCHANGED since the
# previous reload, so a width dragged after that reload has to survive. This is
# what catches a caller that keeps diffing against the STARTUP config instead of
# advancing its snapshot -- `size` would read as changed forever.
CFG_SESSIONS_WIDER_TOUCHED = CFG_SESSIONS_WIDER + """
[appearance.unused_marker_section]
"""

STTY_RE = re.compile(r"\b(\d+) (\d+)\b")


def write_config(t, body):
    path = f"{t.rundir}/config/remux/config.toml"
    with open(path, "w") as fh:
        fh.write(body)
    # notify(7) delivers on the OS's own schedule; give it room, then let the
    # client drain the reload and the `FullRender` the resize provokes.
    t.pump(2.5)


def stty_cols(t):
    """Columns the SHELL believes it has, read out of the pane itself.

    The panel frame can land in exactly the right columns while the pane behind
    it was never resized -- that is precisely the failure a `sync_content_rect`
    without its paired `Resize` produces, so the assertion has to come from
    inside the pane.
    """
    t.send("stty size\r", 1.2)
    for row in reversed(t.rows_text()):
        m = STTY_RE.search(row.strip())
        if m and int(m.group(1)) > 5:
            return int(m.group(2))
    return None


def check_width(fails, before, after, expected, what):
    """Assert a width transition, and FAIL rather than skip on an unread size.

    `stty_cols` returns None when it cannot find the size line -- a prompt that
    scrolled, a slow reload, a panel painted over the row. A comparison guarded
    by `is not None` turns every one of those into a silent pass, and the width
    is the whole point of these tests: the panel frame can be in exactly the
    right columns while the pane behind it was never resized.
    """
    if before is None or after is None:
        fails.append(
            f"could not read `stty size` across {what} (before={before}, "
            f"after={after}) -- the width claim was never checked"
        )
        return
    if after != expected:
        fails.append(
            f"the pane width is wrong across {what}: {before} -> {after} "
            f"columns, expected {expected}"
        )
    else:
        print(f"  across {what} the pane went from {before} to {after} columns")


def panel_rows(t):
    """Non-blank text in the left sidebar band."""
    return [r[:SIDEBAR_W].rstrip() for r in t.rows_text() if r[:SIDEBAR_W].strip()]


def finish(t, name, fails):
    log = t.log("client")
    if "panicked at" in log:
        fails.append("the client log has a panic:\n" + log[-1500:])
    if not t.alive():
        fails.append("the client is not alive")
    t.kill()
    if fails:
        print(f"FAIL {name}")
        for f in fails:
            print("  -", f)
        return False
    print(f"PASS {name}")
    return True


def test_a_sidebar_added_to_the_config_appears_without_a_restart():
    name = "test_a_sidebar_added_to_the_config_appears_without_a_restart"
    t = Tui("/tmp/rmx-sbrl1", cols=COLS, rows=ROWS, config=CFG_NONE).start()
    fails = []
    t.pump(1.0)

    if t.has("Sessions"):
        fails.append("a panel was painted before the config declared one")
    before = stty_cols(t)

    write_config(t, CFG_SESSIONS)

    if not t.has("Sessions"):
        fails.append("the panel never appeared after the config reload")
        t.dump("after reload")
    # The frame is not enough: an empty panel means the tree subscription was
    # not reconciled, which is its own half of this fix.
    rows = panel_rows(t)
    if not any("local" in r for r in rows):
        fails.append(f"the rebuilt panel has no session-tree rows: {rows}")

    after = stty_cols(t)
    check_width(
        fails,
        before,
        after,
        None if before is None else before - SIDEBAR_W,
        "the sidebar being added",
    )

    return finish(t, name, fails)


def test_a_sidebar_removed_from_the_config_goes_away_without_a_restart():
    name = "test_a_sidebar_removed_from_the_config_goes_away_without_a_restart"
    t = Tui("/tmp/rmx-sbrl2", cols=COLS, rows=ROWS, config=CFG_SESSIONS).start()
    fails = []
    t.pump(1.0)

    if not t.has("Sessions"):
        fails.append("the configured panel did not paint at startup")
    before = stty_cols(t)

    write_config(t, CFG_NONE)

    if t.has("Sessions"):
        fails.append("the panel survived its removal from the config")
        t.dump("after removal")
    after = stty_cols(t)
    check_width(
        fails,
        before,
        after,
        None if before is None else before + SIDEBAR_W,
        "the sidebar being removed",
    )

    # The subscription has to be given back. The loop-top reconcile only ever
    # ADDS, so without an explicit `UnsubscribeSessionTree` the server keeps
    # pushing a full tree on every structural change, to nobody, for the rest
    # of the client's life.
    srv = t.log("server")
    if "msg=UnsubscribeSessionTree" not in srv:
        fails.append(
            "the session-tree subscription was never given back after the last "
            "sessions panel went away"
        )

    return finish(t, name, fails)


def test_a_reload_keeps_the_width_the_user_set_by_hand():
    """`sidebar_state` is re-applied after the rebuild, so an unrelated config
    edit does not throw away a width the user dragged out at runtime."""
    name = "test_a_reload_keeps_the_width_the_user_set_by_hand"
    t = Tui("/tmp/rmx-sbrl3", cols=COLS, rows=ROWS, config=CFG_SESSIONS).start()
    fails = []
    t.pump(1.0)

    start = stty_cols(t)

    # Focus the panel and widen the sidebar, which persists to sidebar.json.
    widen_the_sidebar(t)
    widened = stty_cols(t)
    if start is None or widened is None:
        fails.append(
            f"could not read `stty size` around the resize "
            f"(start={start}, widened={widened})"
        )
    elif widened >= start:
        fails.append(f"the sidebar was never widened: {start} -> {widened}")

    write_config(t, CFG_SESSIONS_TOUCHED)

    after = stty_cols(t)
    # Both `None` compares equal, so this needs its own guard too.
    check_width(fails, widened, after, widened, "an unrelated config edit")
    if not t.has("Sessions"):
        fails.append("the panel vanished across the reload")
    # The panel that was already there is rebuilt too, so its plugin starts
    # with NO tree. `wants_session_tree` was already true, so the loop-top
    # reconcile has nothing new to subscribe -- only forgetting the existing
    # subscriptions makes the server re-answer and refill the panel. The header
    # paints either way, so this has to assert on the ROWS.
    rows = panel_rows(t)
    if not any("local" in r for r in rows):
        fails.append(f"the rebuilt panel was left with no tree rows: {rows}")

    return finish(t, name, fails)


def widen_the_sidebar(t):
    """Drag the sidebar wider at runtime, which writes `sidebar.json`."""
    t.send(b"\x1bh", 0.8)          # Alt+h enters the left sidebar
    t.prefix(b"pRl", 1.5)          # Resize right: the sidebar's own edge
    t.send(b"\x1b", 0.6)           # leave the sticky Resize group
    t.send(b"\x1bl", 0.8)          # Alt+l back to the content


def test_a_size_typed_into_the_config_beats_the_persisted_one():
    """The precedence rule's whole point. `sidebar.json` holds a size the
    moment anyone resizes, so without "config wins on edit" every later
    `size = ...` in the config is dead -- and hot-reload made that WORSE, since
    the reload now fires and silently reverts the width."""
    name = "test_a_size_typed_into_the_config_beats_the_persisted_one"
    t = Tui("/tmp/rmx-sbrl4", cols=COLS, rows=ROWS, config=CFG_SESSIONS).start()
    fails = []
    t.pump(1.0)

    # The fixture has to be asserted, not assumed: if the resize chord ever
    # no-ops, no state file is written and there is nothing for the config to
    # beat -- phase 1 would pass having tested nothing.
    start = stty_cols(t)
    widen_the_sidebar(t)
    dragged = stty_cols(t)
    if start is None or dragged is None:
        fails.append(
            f"could not read `stty size` around the resize "
            f"(start={start}, dragged={dragged})"
        )
    elif dragged >= start:
        fails.append(
            f"the sidebar was never widened, so nothing was persisted for the "
            f"config to beat: {start} -> {dragged}"
        )

    write_config(t, CFG_SESSIONS_WIDER)

    after = stty_cols(t)
    check_width(fails, dragged, after, COLS - WIDER_W - 2, "a retyped size")
    if not t.has("Sessions"):
        fails.append("the panel vanished across the reload")

    # Second reload, and the reason the client has to ADVANCE its old-config
    # snapshot. Drag again, then make an edit that leaves `size` alone: diffed
    # against the config as of the previous reload, `size` is unchanged and the
    # drag survives. Diffed against the STARTUP config it reads as changed
    # forever, and every later drag is snapped back.
    widen_the_sidebar(t)
    dragged_again = stty_cols(t)
    if dragged_again is None or after is None:
        fails.append("could not read `stty size` around the second resize")
    elif dragged_again >= after:
        fails.append(f"the second resize did nothing: {after} -> {dragged_again}")

    write_config(t, CFG_SESSIONS_WIDER_TOUCHED)

    final = stty_cols(t)
    check_width(
        fails, dragged_again, final, dragged_again, "a second, unrelated edit"
    )

    return finish(t, name, fails)


def test_a_visible_flip_typed_into_the_config_beats_the_persisted_one():
    """`visible = false` on a sidebar the user opened at runtime must hide it.

    This is the masking the reviewer called out: the persisted `visible` was
    overwriting the config's unconditionally, so the edit appeared to do
    nothing at all.
    """
    name = "test_a_visible_flip_typed_into_the_config_beats_the_persisted_one"
    t = Tui("/tmp/rmx-sbrl5", cols=COLS, rows=ROWS, config=CFG_SESSIONS).start()
    fails = []
    t.pump(1.0)

    # Close and reopen it, so `sidebar.json` carries `visible = true` for this
    # edge -- state that would otherwise mask the config edit below.
    t.prefix(b"bh", 1.2)
    if t.has("Sessions"):
        fails.append("the toggle did not close the sidebar")
    t.prefix(b"bh", 1.2)
    if not t.has("Sessions"):
        fails.append("the toggle did not reopen the sidebar")
    open_cols = stty_cols(t)

    write_config(t, CFG_SESSIONS_HIDDEN)

    if t.has("Sessions"):
        fails.append("`visible = false` in the config was masked by the saved state")
        t.dump("after the visible flip")
    after = stty_cols(t)
    check_width(
        fails,
        open_cols,
        after,
        None if open_cols is None else open_cols + SIDEBAR_W,
        "a retyped visible = false",
    )

    return finish(t, name, fails)


def test_one_save_is_one_reload():
    """The watcher fires per Create/Modify event, and one editor save typically
    produces several. Undebounced, the whole reload arm -- chrome rebuild,
    subscription give-back and re-subscribe of every connection, plugin state
    wiped, repaint -- ran once per event, with a visible flash each time."""
    name = "test_one_save_is_one_reload"
    t = Tui("/tmp/rmx-sbrl6", cols=COLS, rows=ROWS, config=CFG_NONE).start()
    fails = []
    t.pump(1.0)

    baseline = t.log("client").count("client: config reloaded")

    # Three writes inside one debounce window, spaced the way an editor's
    # write/rename/chmod burst is.
    path = f"{t.rundir}/config/remux/config.toml"
    for _ in range(3):
        with open(path, "w") as fh:
            fh.write(CFG_SESSIONS)
        time.sleep(0.04)
    t.pump(3.0)

    reloads = t.log("client").count("client: config reloaded") - baseline
    if reloads != 1:
        fails.append(f"one burst of saves produced {reloads} reloads, expected 1")
    else:
        print(f"  three writes in one window produced {reloads} reload")
    # ...and it still has to actually take effect.
    if not t.has("Sessions"):
        fails.append("the coalesced reload did not apply the config")

    return finish(t, name, fails)


def panel_marker(t):
    """The placeholder's focus marker as it currently renders, or None."""
    for row in t.rows_text():
        cell = row[:SIDEBAR_W]
        if "focused" in cell:
            return "focused"
        if "idle" in cell:
            return "idle"
    return None


def test_a_reload_does_not_yank_the_keyboard_out_of_a_focused_panel():
    """An unrelated config edit while the user is working inside a panel must
    not hand the keyboard back to the content -- to them that is a dropped
    keypress with no cause on screen. The rebuild renumbers the sidebars, so
    the focus has to be carried across by EDGE, not by index."""
    name = "test_a_reload_does_not_yank_the_keyboard_out_of_a_focused_panel"
    t = Tui("/tmp/rmx-sbrl7", cols=COLS, rows=ROWS, config=CFG_PLACEHOLDER).start()
    fails = []
    t.pump(1.0)

    if panel_marker(t) != "idle":
        fails.append(f"the panel did not start unfocused: {panel_marker(t)!r}")
    t.send(b"\x1bh", 1.0)          # Alt+h enters the left sidebar
    if panel_marker(t) != "focused":
        fails.append(f"Alt+h did not focus the panel: {panel_marker(t)!r}")

    write_config(t, CFG_PLACEHOLDER_TOUCHED)

    if panel_marker(t) != "focused":
        fails.append(
            f"the reload yanked the keyboard out of the panel: {panel_marker(t)!r}"
        )
        t.dump("after the reload")
    # And it is real focus, not just a marker: a plain key must still reach the
    # plugin rather than the shell.
    t.send(b"j", 0.8)
    if not any("focused 1" in r[:SIDEBAR_W] for r in t.rows_text()):
        fails.append(
            "a plain key did not reach the panel after the reload: "
            + repr([r[:SIDEBAR_W].rstrip() for r in t.rows_text() if r[:SIDEBAR_W].strip()])
        )

    return finish(t, name, fails)


def panel_markers(t):
    """(row, "focused"/"idle") for every placeholder marker on screen."""
    out = []
    for y, row in enumerate(t.rows_text()):
        cell = row[:SIDEBAR_W]
        if "focused" in cell:
            out.append((y, "focused"))
        elif "idle" in cell:
            out.append((y, "idle"))
    return out


def test_focus_falls_back_when_the_reload_drops_the_focused_panel():
    """The focused panel can survive in `panels` and vanish from `panel_rects`.

    `split_panels` drops a panel whose weighted share falls below its
    `min_size`, so a stack the user just reweighted can leave the restored
    index naming something that is never painted. Restoring focus there
    swallows the keyboard into a panel nobody can see.
    """
    name = "test_focus_falls_back_when_the_reload_drops_the_focused_panel"
    t = Tui("/tmp/rmx-sbrl8", cols=COLS, rows=ROWS, config=CFG_STACKED).start()
    fails = []
    t.pump(1.0)

    # A regressed fixture must FAIL this test, not abort the whole run: the
    # index below is unconditional, so a stack that starts with one panel
    # would raise out of the harness with a traceback and take the remaining
    # tests with it.
    if len(panel_markers(t)) != 2:
        fails.append(f"the stack did not start with two panels: {panel_markers(t)}")
        return finish(t, name, fails)

    t.send(b"\x1bh", 1.0)          # Alt+h enters the sidebar, on panel 0
    t.send(b"\x1bj", 1.0)          # Alt+j walks down to panel 1
    marks = panel_markers(t)
    focused = [y for (y, m) in marks if m == "focused"]
    if len(marks) < 2 or focused != [marks[1][0]]:
        fails.append(f"the SECOND panel never took focus: {marks}")
        return finish(t, name, fails)

    write_config(t, CFG_STACKED_SECOND_DROPPED)

    marks = panel_markers(t)
    if len(marks) != 1:
        fails.append(f"the reweighted stack did not drop a panel: {marks}")
    if marks and marks[0][1] != "focused":
        fails.append(
            f"focus was restored to the panel that is no longer painted: {marks}"
        )
    # And it is real focus: a plain key has to reach the surviving plugin.
    t.send(b"j", 0.8)
    if not any("focused 1" in r[:SIDEBAR_W] for r in t.rows_text()):
        fails.append(
            "a plain key did not reach the surviving panel: "
            + repr([r[:SIDEBAR_W].rstrip() for r in t.rows_text() if r[:SIDEBAR_W].strip()])
        )

    return finish(t, name, fails)


def read_state(t):
    path = f"{t.rundir}/state/remux/sidebar.json"
    return open(path).read() if os.path.exists(path) else ""


def test_commenting_a_block_out_and_back_in_returns_the_dragged_width():
    """The reload's save-back must not erase state for an edge the config just
    dropped -- `SidebarState::from_chrome` describes only what is configured
    now, so writing it verbatim destroys the rest. Commenting a block out to
    try something without it reads as reversible; it has to be."""
    name = "test_commenting_a_block_out_and_back_in_returns_the_dragged_width"
    t = Tui("/tmp/rmx-sbrl9", cols=COLS, rows=ROWS, config=CFG_SESSIONS).start()
    fails = []
    t.pump(1.0)

    # The width claim below is self-referential -- `back == dragged` -- so it
    # passes on a dead fixture where both are the config default. The
    # state-file check is not a substitute: it only discriminates because a
    # no-op resize writes no `sidebar.json` at all, and it would report an
    # ERASURE that never happened. Anything else that starts writing the file
    # (persisting `focused_panel`, say -- explicitly left out today) retires
    # that accident and this scenario would pass having tested nothing.
    start = stty_cols(t)
    widen_the_sidebar(t)
    dragged = stty_cols(t)
    if start is None or dragged is None:
        fails.append(
            f"could not read `stty size` around the resize "
            f"(start={start}, dragged={dragged})"
        )
    elif dragged >= start:
        fails.append(
            f"the sidebar was never widened, so the round trip has nothing to "
            f"return: {start} -> {dragged}"
        )

    write_config(t, CFG_NONE)          # the block commented out
    state = read_state(t)
    if '"Left"' not in state and '"left"' not in state:
        fails.append(
            "the reload's save-back erased the dropped edge's state: " + repr(state)
        )

    write_config(t, CFG_SESSIONS)      # and back in
    back = stty_cols(t)
    check_width(fails, dragged, back, dragged, "a block commented out and back in")

    return finish(t, name, fails)


if __name__ == "__main__":
    from pty_harness import BIN

    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    ok = True
    for test in (
        test_a_sidebar_added_to_the_config_appears_without_a_restart,
        test_a_sidebar_removed_from_the_config_goes_away_without_a_restart,
        test_a_reload_keeps_the_width_the_user_set_by_hand,
        test_a_size_typed_into_the_config_beats_the_persisted_one,
        test_a_visible_flip_typed_into_the_config_beats_the_persisted_one,
        test_one_save_is_one_reload,
        test_a_reload_does_not_yank_the_keyboard_out_of_a_focused_panel,
        test_focus_falls_back_when_the_reload_drops_the_focused_panel,
        test_commenting_a_block_out_and_back_in_returns_the_dragged_width,
    ):
        ok = test() and ok
    print("ALL PASS" if ok else "FAILURES")
    sys.exit(0 if ok else 1)
