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
    t.send(b"\x1bh", 0.8)          # Alt+h enters the left sidebar
    t.prefix(b"pRl", 1.5)          # Resize right: the sidebar's own edge
    t.send(b"\x1b", 0.6)           # leave the sticky Resize group
    t.send(b"\x1bl", 0.8)          # Alt+l back to the content
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


if __name__ == "__main__":
    from pty_harness import BIN

    if not os.path.exists(BIN):
        sys.exit(f"build first: {BIN} missing")
    ok = True
    for test in (
        test_a_sidebar_added_to_the_config_appears_without_a_restart,
        test_a_sidebar_removed_from_the_config_goes_away_without_a_restart,
        test_a_reload_keeps_the_width_the_user_set_by_hand,
    ):
        ok = test() and ok
    print("ALL PASS" if ok else "FAILURES")
    sys.exit(0 if ok else 1)
