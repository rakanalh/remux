#!/usr/bin/env python3
"""A view's status bar must be styled EXACTLY like a normal tab's.

Sibling of `view_border_parity.py`, for the OTHER piece of shared chrome. That
test deliberately SKIPS the bottom row (it is server-rendered in a normal tab
and client-rendered in a view); this test reads only the bottom row.

The bug this guards: `view::draw_status_bar` claimed in its own doc comment to
mirror the server bar "so the colors match the normal bar exactly", and the LEFT
half genuinely did -- but the RIGHT half (the layout indicator) was drawn from
`session_name_fg` + `status_bar_bg` + bold, while the server draws it black on
grey, NOT bold. Entering a view visibly flipped the layout indicator from
black-on-grey to teal-bold-on-mantle.

Everything is compared inside ONE client, so the theme is necessarily identical:

    normal tab (2 panes)   vs   view (2 cells aliasing those same 2 panes)

The two bars carry DIFFERENT text (a normal tab shows `bsp`, a view defaults to
`grid`), so column ranges are not comparable -- the segments are located
independently on each side and their (fg, bg, bold) style triples compared.

Run from the repo root:  python3 tests/pty/view_status_bar_parity.py
"""
import sys
from pty_harness import Tui

MARK_A = "AAAA_status_marker"
MARK_B = "BBBB_status_marker"

LAYOUT_NAMES = {"bsp", "master", "monocle", "grid", "custom"}


def style(cell):
    return (str(cell.fg), str(cell.bg), bool(cell.bold))


def right_segment(t):
    """(text, style) of the status bar's right-hand segment.

    The segment is the run of cells ending at the last column that share the
    last column's style -- exactly how both renderers paint it (one contiguous
    right-aligned run). Returning the style as ONE triple also asserts, by
    construction, that the segment is uniformly styled.
    """
    row = t.screen.buffer[t.screen.lines - 1]
    last = t.screen.columns - 1
    want = style(row[last])
    x = last
    while x >= 0 and style(row[x]) == want:
        x -= 1
    text = "".join(row[c].data for c in range(x + 1, last + 1))
    return text, want


def make_two_panes(t):
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_A}\\n'\r", 0.5)
    t.prefix(b"pv", 0.7)
    t.send("clear\r", 0.4)
    t.send(f"printf '{MARK_B}\\n'\r", 0.6)


def compose_view(t):
    t.prefix(b"xm", 0.8)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.5)
    t.send("j", 0.2); t.send(" ", 0.3)
    t.send("j", 0.2); t.send(" ", 0.3)
    t.send("v", 0.3); t.send("a", 0.7)
    t.send("\r", 1.5)
    t.pump(0.8)


def require_in_view(t):
    """Hard gate: abort unless a VIEW is really on screen (see
    view_border_parity.py -- without this the comparison is the normal tab
    against itself and passes trivially)."""
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    if "View 1" not in t.rows_text()[-1]:
        reasons.append(f"status bar is not a view bar: {t.rows_text()[-1].rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title on any border")
    if reasons:
        print("ABORT: never entered the view -- the comparison would be meaningless:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a view")
        t.kill()
        sys.exit(1)


def main():
    t = Tui("/tmp/rmxsbp", cols=120, rows=40).start()
    fails = []
    try:
        make_two_panes(t)
        if not (t.has(MARK_A) and t.has(MARK_B)):
            print("ABORT: the two panes were not created")
            t.dump("no two panes")
            t.kill()
            sys.exit(1)
        normal_text, normal_style = right_segment(t)
        print(f"normal tab : right segment {normal_text!r} style={normal_style}")

        compose_view(t)
        require_in_view(t)
        view_text, view_style = right_segment(t)
        print(f"view       : right segment {view_text!r} style={view_style}")

        # Each side must really be showing ITS OWN layout indicator, so a style
        # match cannot be an artifact of both segments being blank padding.
        if normal_text.strip() not in LAYOUT_NAMES:
            fails.append(f"normal tab's right segment {normal_text!r} is not a layout name")
        if view_text.strip() not in LAYOUT_NAMES:
            fails.append(f"view's right segment {view_text!r} is not a layout name")

        if view_style != normal_style:
            fails.append(
                f"right-segment STYLE mismatch: view={view_style} "
                f"normal={normal_style} (fg, bg, bold) -- the view's layout "
                "indicator is not drawn by the shared status-bar renderer")

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
        print("RESULT: PASS")
    finally:
        t.kill()


if __name__ == "__main__":
    main()
