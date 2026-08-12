#!/usr/bin/env python3
"""View cells must be framed EXACTLY like normal panes, and `Ctrl-a g`
(ToggleStyle) must work while a view is displayed.

The bug this guards: `draw_cell` used to draw its own borders with SQUARE
corners `┌┐└┘` and two hardcoded colors (`Indexed(10)`/`Indexed(8)`), while the
server's zellij renderer draws ROUNDED corners `╭╮╰╯` from the theme's
`frame_active_fg`/`frame_fg` roles -- so a view's borders did not match a normal
tab's, and `ToggleStyle` had no effect at all inside a view. Grid is the default
layout for views, which is why the user reported this as "the grid layout
borders don't match the other layouts".

Everything is compared inside ONE client, so the theme is necessarily identical:

    normal tab (2 panes)   vs   view (2 cells aliasing those same 2 panes)

asserting that the set of box-drawing glyphs AND the set of border foreground
colors match, then toggling the style twice and asserting the view follows.

Color parity is the crux: the hardcoded `Indexed(10)`/`Indexed(8)` pair matched
no theme, so a glyph-only check would have missed half the bug.

Run from the repo root:  python3 tests/pty/view_border_parity.py
"""
import sys
from pty_harness import Tui

ROUNDED = ["╭", "╮", "╰", "╯"]  # ╭ ╮ ╰ ╯
SQUARE = ["┌", "┐", "└", "┘"]   # ┌ ┐ └ ┘
EDGES = ["─", "│"]                        # ─ │
BOX = set(ROUNDED + SQUARE + EDGES)

MARK_A = "AAAA_border_marker"
MARK_B = "BBBB_border_marker"


def caps(t):
    """(glyph set, border fg-color set) of the box-drawing cells on screen.

    BOTH sets skip the bottom row: it is the server-rendered status bar in a
    normal tab but the CLIENT-rendered one in a view (`view::draw_status_bar`),
    and both happen to contain a `│` separator. Including it would compare two
    unrelated pieces of chrome, so a status-bar difference could read as a
    border-parity failure.
    """
    glyphs, colors = set(), set()
    buf = t.screen.buffer
    for y in range(t.screen.lines - 1):
        row = buf[y]
        for x in range(t.screen.columns):
            cell = row[x]
            if cell.data in BOX:
                glyphs.add(cell.data)
                colors.add(str(cell.fg))
    return glyphs, colors


def make_two_panes(t):
    """A tab with 2 panes, each carrying a distinct marker."""
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.7)                                   # split vertical
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_B}\\n'\r", 0.6)


def compose_view(t):
    """Mark both panes in the session manager and alias them into a new view."""
    t.prefix(b"xm", 0.8)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    t.send(b"\t", 0.3)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.5)   # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 2
    t.send("v", 0.3); t.send("a", 0.7)                     # AddToView picker
    t.send("\r", 1.5)                                      # "New view" -> enter
    t.pump(0.8)                                            # let PaneContent land


def require_in_view(t):
    """Hard gate: abort unless a VIEW is really on screen.

    Without this the whole comparison can pass while never exercising the view
    at all -- if composition silently fails, the normal tab is still showing, so
    glyph/color parity is trivially true, and `Ctrl-a g` still flips the normal
    tab through the server-side path that was never broken. Note the `Add Pane
    to View` picker itself draws ROUNDED corners and contains the word "View",
    so neither a corner check nor a naive substring search discriminates.

    Signals, all of which only a live view produces:
      * no session-manager / picker overlay is still up;
      * the client-side view status bar (`[MODE]  <view> │ <cell title>`), which
        a normal tab's server status bar (`[MODE]  <session> │ <tab>`) never
        shows -- the view name is the discriminator;
      * a cell's top-border label is `cell_title` = "<session> / Tab <n>",
        whereas a normal pane's border is labelled with the process ("sh").
    """
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    status = t.rows_text()[-1]
    if "View 1" not in status:
        reasons.append(f"status bar is not a view status bar: {status.rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title ('<session> / Tab 1') on any border")
    if t.has("╭ sh"):
        reasons.append("a normal pane border ('╭ sh') is still on screen")
    if reasons:
        print("ABORT: never entered the view -- the comparison would be "
              "meaningless (it would compare the normal tab against itself):")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a view")
        t.kill()
        sys.exit(1)


def main():
    t = Tui("/tmp/rmxvbp", cols=120, rows=40).start()
    fails = []
    try:
        # --- 1. normal tab: 2 panes ------------------------------------------
        make_two_panes(t)
        normal_glyphs, normal_colors = caps(t)
        print(f"normal tab : glyphs={sorted(normal_glyphs)}")
        print(f"normal tab : colors={sorted(normal_colors)}")
        if not (t.has(MARK_A) and t.has(MARK_B)):
            print("ABORT: the two panes were not created")
            t.dump("no two panes")
            t.kill()
            sys.exit(1)

        # --- 2. view over those same 2 panes ---------------------------------
        compose_view(t)
        require_in_view(t)
        view_glyphs, view_colors = caps(t)
        print(f"view       : glyphs={sorted(view_glyphs)}")
        print(f"view       : colors={sorted(view_colors)}")

        # The cells really alias the two panes (not a stale normal-tab paint).
        if not (t.has(MARK_A) and t.has(MARK_B)):
            fails.append("the view is not streaming both aliased panes' content")

        # --- 3. parity -------------------------------------------------------
        missing = [c for c in ROUNDED if c not in view_glyphs]
        if missing:
            fails.append(f"view is missing rounded corners {missing}; "
                         f"got {sorted(view_glyphs)}")
        squares = [c for c in SQUARE if c in view_glyphs]
        if squares:
            fails.append(f"view still draws SQUARE corners {squares} "
                         "(the old hardcoded draw_cell borders)")
        if view_glyphs != normal_glyphs:
            fails.append(f"glyph set mismatch: view={sorted(view_glyphs)} "
                         f"normal={sorted(normal_glyphs)}")
        if view_colors != normal_colors:
            fails.append(f"border COLOR set mismatch: view={sorted(view_colors)} "
                         f"normal={sorted(normal_colors)} -- view borders are not "
                         "drawn from the theme's frame roles")

        # --- 4. ToggleStyle inside the view: -> tmux style -------------------
        t.prefix(b"g", 1.2)
        t.pump(0.5)
        tmux_glyphs, tmux_colors = caps(t)
        print(f"view+tmux  : glyphs={sorted(tmux_glyphs)}")
        print(f"view+tmux  : colors={sorted(tmux_colors)}")
        corners = [c for c in ROUNDED + SQUARE if c in tmux_glyphs]
        if corners:
            fails.append(f"view did not flip to tmux style; corners remain: {corners}")
        if tmux_glyphs == view_glyphs:
            fails.append("Ctrl-a g had NO effect inside the view "
                         "(frame is byte-identical to before the toggle)")

        # --- 5. toggle back: rounded zellij frame restored -------------------
        t.prefix(b"g", 1.2)
        t.pump(0.5)
        back_glyphs, back_colors = caps(t)
        print(f"view+back  : glyphs={sorted(back_glyphs)}")
        print(f"view+back  : colors={sorted(back_colors)}")
        if back_glyphs != view_glyphs or back_colors != view_colors:
            fails.append(f"toggling back did not restore the zellij frame: "
                         f"glyphs={sorted(back_glyphs)} colors={sorted(back_colors)}")

        # --- 6. a view's style is CLIENT-LOCAL: it must not mutate the session
        # `Session::border_style` is per-session server state, and a view's cells
        # alias panes across several sessions/machines that each own their own
        # style -- so a view has no single session style to mirror, and toggling
        # inside a view must leave the underlying session's style alone. Closing
        # the view therefore hands back a session still drawn in ITS own style
        # (zellij here), regardless of what the view was toggled to.
        t.prefix(b"g", 1.2)          # -> tmux, inside the view
        t.prefix(b"wq", 1.2)         # close the view, re-attach the session
        t.pump(0.8)
        closed_glyphs, closed_colors = caps(t)
        print(f"after close: glyphs={sorted(closed_glyphs)} "
              f"colors={sorted(closed_colors)} (want the session's own zellij frame)")
        if closed_glyphs != normal_glyphs or closed_colors != normal_colors:
            fails.append(f"toggling the style inside a view leaked into the "
                         f"session: after closing, the session frame is "
                         f"glyphs={sorted(closed_glyphs)} colors={sorted(closed_colors)}, "
                         f"expected its own untouched glyphs={sorted(normal_glyphs)} "
                         f"colors={sorted(normal_colors)}")

        # --- 7. liveness -----------------------------------------------------
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
            t.dump("final")
            print("RESULT: FAIL")
            sys.exit(1)
        print("\nRESULT: PASS -- view cells are framed identically to normal "
              "panes (same rounded glyphs, same theme frame colors) and "
              "ToggleStyle works inside a view")
    finally:
        t.kill()


if __name__ == "__main__":
    main()
