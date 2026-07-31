#!/usr/bin/env python3
"""The command palette lists -- and can actually run -- every bindable action.

Two bugs, one harness:

  * `EnterSearchMode` was parseable by `parse_command` but missing from
    `command_names()`, the list the palette is built from. You could bind it,
    but you could not find it.
  * The nine client-only actions (`ViewNew`, `SessionQuickSwitch`, ...) were
    matched as bare literals in the input handler and appeared in NO registry,
    so the palette neither listed them nor could have run them -- it dispatched
    server commands only.

Both now come from one registry (`protocol::action_specs`), and confirming a
palette entry runs the SAME action chain a keybinding does. A unit test can
check the listing; only a real PTY can check that picking one performs it and
that the palette overlay is torn down when the picked action opens a different
overlay.

Run from the repo root:
    python3 tests/pty/command_palette_actions.py
"""
import sys
from pty_harness import Tui

RUNDIR = "/tmp/rmxcpa"


def open_palette(t):
    """Prefix + ':' opens the command palette."""
    t.prefix(b":", 0.6)


def type_filter(t, text):
    t.send(text.encode(), 0.5)


def run():
    fails = []
    t = Tui(RUNDIR).start()
    try:
        # -- 1. EnterSearchMode is now findable ---------------------------
        open_palette(t)
        if not t.has("Command Palette"):
            fails.append("palette did not open on Ctrl-a :")
            t.dump("no palette")
        type_filter(t, "EnterSearch")
        if not t.has("EnterSearchMode"):
            fails.append("EnterSearchMode is not listed in the palette")
            t.dump("EnterSearchMode missing")
        t.send(b"\x1b", 0.4)  # Esc

        # -- 2. The client-only actions are listed ------------------------
        # Type a strict PREFIX of each name: the full name can then only come
        # from the listing, never from the echoed input line.
        for prefix, name in (
            ("ViewN", "ViewNew"),
            ("SessionQuick", "SessionQuickSwitch"),
            ("ViewLayout", "ViewLayoutNext"),
            ("CommandPalette", "CommandPaletteOpen"),
            ("ViewDel", "ViewDelete"),
        ):
            open_palette(t)
            type_filter(t, prefix)
            if not t.has(name):
                fails.append(f"client action {name} is not listed in the palette")
                t.dump(f"{name} missing")
            t.send(b"\x1b", 0.4)

        # -- 3. Picking a client-only action PERFORMS it ------------------
        # ViewNew prompts for a view name via the rename overlay.
        open_palette(t)
        type_filter(t, "ViewNew")
        t.send(b"\r", 0.8)  # Enter
        if not t.has("New View"):
            fails.append("selecting ViewNew did not start the new-view prompt")
            t.dump("ViewNew not performed")
        # ... and the palette it was picked from is gone, not painted underneath.
        if t.has("Command Palette"):
            fails.append("palette overlay survived the ViewNew prompt")
            t.dump("palette still painted")
        t.send(b"\x1b", 0.4)  # cancel the prompt

        # SessionQuickSwitch opens the session switcher overlay.
        open_palette(t)
        type_filter(t, "SessionQuickSwitch")
        t.send(b"\r", 0.8)
        if not t.has("Switch Session"):
            fails.append("selecting SessionQuickSwitch did not open the switcher")
            t.dump("SessionQuickSwitch not performed")
        if t.has("Command Palette"):
            fails.append("palette overlay survived the session switcher")
            t.dump("palette still painted (switcher)")
        t.send(b"\x1b", 0.4)

        # -- 3b. Picking EnterSearchMode actually enters Search mode -------
        open_palette(t)
        type_filter(t, "EnterSearch")
        t.send(b"\r", 0.8)
        if not t.has("SEARCH"):
            fails.append("selecting EnterSearchMode did not enter Search mode")
            t.dump("EnterSearchMode not performed")
        t.send(b"\x1b", 0.4)

        # -- 4. A server command from the palette still works --------------
        # (the path that already worked must not regress: split the pane)
        open_palette(t)
        type_filter(t, "PaneSplitVertical")
        t.send(b"\r", 0.8)
        if t.has("Command Palette"):
            fails.append("palette overlay survived a server command")
            t.dump("palette still painted (server cmd)")

        alive = t.alive()
        logs = (t.log("client") + t.log("server")).lower()
        panic = "panic" in logs
        print(f"alive={alive} panic={panic}")
        if not alive:
            fails.append("client died")
        if panic:
            fails.append("panic in the logs")

        if fails:
            print("\nFAILURES:")
            for f in fails:
                print(f"  - {f}")
            print("RESULT: FAIL")
            return 1
        print("RESULT: PASS")
        return 0
    finally:
        t.kill()


if __name__ == "__main__":
    sys.exit(run())
