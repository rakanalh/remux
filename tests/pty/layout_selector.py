#!/usr/bin/env python3
"""The layout switcher (Alt-g), and Alt+Shift+H/J/K/L moving panes.

A real PTY, because the switcher is a CLIENT overlay opened by a key the client
decodes: the server only answers `ListLayouts` / `ListViewLayouts` and applies
`LayoutSet` / `ViewSetLayout`. The
move keys are here too because they share the decoding fix: crossterm reports
`ESC L` as `'L'` + Alt + Shift, and before `NormalizedKeyEvent` dropped that
Shift, no Alt+Shift+letter shortcut ever fired.

What it covers:

  1  `ESC g` opens the switcher
  2  the list is the SERVER's `[layouts]`: `grid = false` leaves grid out, the
     other five automatic layouts are listed, and the current one is marked
  3  Enter applies the highlighted layout: the status bar's layout name, which
     the SERVER composites, changes to it
  4  a `LayoutSet grid` bound in config is refused by the server, so the client
     never decides which layouts exist
  5  after a manual resize (which ejects the tab to Custom) and a switch away,
     `custom` is offered and restores
  6  Alt+Shift+H/J/K/L move the focused pane, in every encoding a terminal
     sends (legacy `ESC L`, CSI-u base key `CSI 108;4u`, CSI-u shifted key
     `CSI 76;4u`), and none of them opens the switcher. The pane is tracked by
     a marker its own shell printed (`printf 'P%s' B` typed, `PB` searched),
     so the typed command cannot fake it
  7  the client is alive and neither log has a panic
  8  with the popup terminal open (`Alt-p`) the server offers no layouts, the
     switcher says why, and Enter closes it without changing the layout; once
     the popup is closed the list comes back
  9  in a View with two cells side by side, the switcher lists the View's
     layouts (grid absent, the current one marked); picking `rows` stacks the
     cells, judged by the `stty size` each cell's own shell reports; the tab
     underneath keeps its layout; server.log shows `ListViewLayouts` and
     `ViewSetLayout` and no `ListLayouts` or `LayoutSet`; the client stays
     alive

Run from the repo root:
    python3 tests/pty/layout_selector.py [-v]
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import PROTOCOL_VERSION, Tui, sm_compose_view  # noqa: E402

RUN = "/tmp/rmx-lsel"
VERBOSE = "-v" in sys.argv

OPEN = b"\x1bg"
POPUP_TOGGLE = b"\x1bp"


def encodings(letter):
    """Every byte form of Alt+Shift+`letter`: legacy, CSI-u base, CSI-u shifted."""
    return [
        (f"ESC {letter.upper()}", b"\x1b" + letter.upper().encode()),
        (f"CSI {ord(letter)};4u", f"\x1b[{ord(letter)};4u".encode()),
        (f"CSI {ord(letter.upper())};4u", f"\x1b[{ord(letter.upper())};4u".encode()),
    ]

EXPECTED = ["bsp", "master", "monocle", "columns", "rows"]
TITLE = "Switch Layout"

CONFIG = """
[layouts]
grid = false

[keybindings.command]
"Alt-u" = "LayoutSet grid"
"Alt-y" = "LayoutSet columns"
"Alt-i" = "LayoutSet rows"
"Alt-r" = "ResizeRight 3"
"""

FAILURES = []


def check(name, ok, detail=""):
    if ok:
        print(f"  ok    {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def popup_rows(t):
    """The item rows inside the switcher box, stripped of border and marker."""
    rows = t.rows_text()
    top = next((i for i, r in enumerate(rows) if TITLE in r), -1)
    if top < 0:
        return None
    left = rows[top].index("╭")
    items = []
    for r in rows[top + 1:]:
        cell = r[left:]
        if cell.startswith("╰"):
            break
        inner = cell[1:].split("│")[0]
        items.append(inner.rstrip())
    return items


def popup_extent(t):
    """(top, bottom, left, right) of the switcher box on screen, or None."""
    rows = t.rows_text()
    top = next((i for i, r in enumerate(rows) if TITLE in r), -1)
    if top < 0:
        return None
    left = rows[top].index("╭")
    right = rows[top].index("╮", left)
    bottom = next((i for i in range(top + 1, len(rows))
                   if rows[i][left:left + 1] == "╰"), -1)
    if bottom < 0:
        return None
    return top, bottom, left, right


def status_layout(t):
    return t.rows_text()[-1].split()[-1]


def pick(t, name, items):
    """Highlight `name` by position (g, then j steps) and press Enter."""
    names = [i[2:] for i in items]
    if name not in names:
        check(f"{name} is offered to pick", False, names)
        return
    t.send(b"g", 0.3)
    for _ in range(names.index(name)):
        t.send(b"j", 0.2)
    t.send(b"\r", 1.0)


def marker_pos(t, marker):
    """(row, col) of every line-start occurrence of `marker` on screen.

    `finditer`, because two panes share each screen row and the marker can sit
    in either half.
    """
    import re
    hits = []
    for y, row in enumerate(t.rows_text()):
        for m in re.finditer(re.escape(marker) + r"\b", row):
            hits.append((y, m.start()))
    return hits


def switcher_section():
    t = Tui(RUN, config=CONFIG).start()
    try:
        current = status_layout(t)
        check("starts in bsp", current == "bsp", t.rows_text()[-1])

        for target in ["monocle", "columns", "rows"]:
            label = f"ESC g -> {target}"
            t.send(OPEN, 1.0)
            items = popup_rows(t)
            if VERBOSE:
                t.dump(label)
            check(f"{label}: switcher opens", items is not None,
                  "\n".join(t.rows_text()))
            if items is None:
                continue
            names = [i[2:] for i in items]
            check(f"{label}: lists the enabled layouts in cycle order",
                  names == EXPECTED, names)
            check(f"{label}: grid is not offered", "grid" not in names, names)
            marked = [i[2:] for i in items if i.startswith("* ")]
            check(f"{label}: marks the current layout",
                  marked == [current], f"marked={marked} current={current}")
            pick(t, target, items)
            check(f"{label}: closes on Enter", popup_rows(t) is None,
                  "\n".join(t.rows_text()))
            current = status_layout(t)
            check(f"{label}: status bar shows {target}", current == target,
                  t.rows_text()[-1])

        t.send(b"\x1bu", 1.0)
        check("LayoutSet grid is refused by the server",
              status_layout(t) == current, t.rows_text()[-1])
        check("the refusal is logged", 'LayoutSet "grid" refused' in t.log("server"),
              t.log("server")[-800:])

        # The positive control for the refusal above: the same binding path with
        # an enabled layout does change the status bar.
        t.send(b"\x1by", 1.0)
        current = status_layout(t)
        check("LayoutSet columns from a binding applies", current == "columns",
              t.rows_text()[-1])

        t.prefix(b"pv", 1.0)
        t.send(b"\x1br", 1.0)
        current = status_layout(t)
        check("a manual resize ejects to custom", current == "custom",
              t.rows_text()[-1])
        t.send(OPEN, 1.0)
        items = popup_rows(t) or []
        names = [i[2:] for i in items]
        check("custom is listed and marked while the tab is custom",
              "* custom" in items, items)
        pick(t, "bsp", items)
        check("switching away from custom", status_layout(t) == "bsp",
              t.rows_text()[-1])
        t.send(OPEN, 1.0)
        items = popup_rows(t) or []
        names = [i[2:] for i in items]
        check("the remembered custom layout is still offered",
              names == EXPECTED + ["custom"], names)
        pick(t, "custom", items)
        check("custom restores", status_layout(t) == "custom", t.rows_text()[-1])

        check("client is alive", t.alive())
    finally:
        t.kill()
    check_logs(t, "switcher")


def move_section():
    t = Tui(RUN, config=CONFIG).start()
    try:
        t.send(b"printf 'P%s\\n' A\r", 0.6)
        t.prefix(b"pv", 1.2)
        t.send(b"printf 'P%s\\n' B\r", 0.8)

        def where():
            a, b = marker_pos(t, "PA"), marker_pos(t, "PB")
            if len(a) != 1 or len(b) != 1:
                return None
            return a[0], b[0]

        start = where()
        check("two panes, each with its marker", start is not None,
              "\n".join(t.rows_text()))
        if start is None:
            return
        check("the focused pane B starts right of A", start[1][1] > start[0][1], start)

        # Each key is checked from a start the PREFIX tree set up (`Ctrl-a p
        # L` etc.), which never goes through the shortcut lookup. A check that
        # started from where the previous key left B would pass vacuously after
        # that key failed to move it.
        def setup(prefix_key, ok, what):
            t.prefix(b"p" + prefix_key, 1.0)
            pos = where()
            if not (pos is not None and ok(pos)):
                check(f"prefix p {prefix_key.decode()} puts B {what}", False, pos)

        def left(p):
            return p[1][1] < p[0][1]

        def above(p):
            return p[1][0] < p[0][0]

        for (lh, h), (ll, l) in zip(encodings("h"), encodings("l")):
            setup(b"L", lambda p: not left(p), "right of A")
            t.send(h, 1.0)
            pos = where()
            check(f"{lh} moves B left of A", pos is not None and left(pos), pos)
            setup(b"H", left, "left of A")
            t.send(l, 1.0)
            pos = where()
            check(f"{ll} moves B right of A", pos is not None and pos[1][1] > pos[0][1],
                  pos)
            check(f"{ll} does not open the switcher", popup_rows(t) is None,
                  "\n".join(t.rows_text()))

        t.send(b"\x1bi", 1.2)
        check("rows layout for the vertical moves", status_layout(t) == "rows",
              t.rows_text()[-1])
        for (lk, k), (lj, j) in zip(encodings("k"), encodings("j")):
            setup(b"J", lambda p: not above(p), "below A")
            t.send(k, 1.0)
            pos = where()
            check(f"{lk} moves B above A", pos is not None and above(pos), pos)
            setup(b"K", above, "above A")
            t.send(j, 1.0)
            pos = where()
            check(f"{lj} moves B below A", pos is not None and pos[1][0] > pos[0][0], pos)
        if VERBOSE:
            t.dump("after moves")
        check("client is alive after the moves", t.alive())
    finally:
        t.kill()
    check_logs(t, "moves")


def popup_section():
    t = Tui(RUN, config=CONFIG).start()
    try:
        before = status_layout(t)
        t.send(POPUP_TOGGLE, 1.2)
        t.send(OPEN, 1.0)
        items = popup_rows(t)
        if VERBOSE:
            t.dump("switcher over the popup")
        check("popup open: the switcher says why it offers nothing",
              items == ["  no layouts: popup open"],
              items if items is not None else "\n".join(t.rows_text()))
        # `j` first, so a list that did arrive would move the highlight off the
        # current layout and Enter would name a different one.
        t.send(b"j", 0.2)
        t.send(b"\r", 1.0)
        check("popup open: Enter closes the switcher", popup_rows(t) is None,
              "\n".join(t.rows_text()))
        check("popup open: the layout is unchanged", status_layout(t) == before,
              t.rows_text()[-1])

        t.send(POPUP_TOGGLE, 1.2)
        t.send(OPEN, 1.0)
        items = popup_rows(t) or []
        check("popup closed: the list comes back",
              [i[2:] for i in items] == EXPECTED, items)
        t.send(b"\x1b", 0.6)
        check("client is alive after the popup checks", t.alive())
    finally:
        t.kill()
    check_logs(t, "popup")


SIZE_RE = re.compile(r"SZ(\d):(\d+) (\d+)")


def cell_sizes(t, phase):
    """Every `(rows, cols)` a cell's shell reported for `phase`, left to right
    and top to bottom. The shell assembles `SZ<phase>:` itself, so the typed
    command never matches.
    """
    hits = []
    for y, row in enumerate(t.rows_text()):
        for m in SIZE_RE.finditer(row):
            if m.group(1) == str(phase):
                hits.append((y, m.start(), int(m.group(2)), int(m.group(3))))
    return sorted(hits)


def report_sizes(t, phase, other_cell):
    """Have each of the View's two cells print its own `stty size`: the focused
    one, then the one `other_cell` (an Alt+direction key) moves focus to.
    """
    cmd = f"printf 'SZ%s:%s\\n' {phase} \"$(stty size)\"\r".encode()
    t.send(cmd, 0.6)
    t.send(other_cell, 0.6)
    t.send(cmd, 0.8)
    return cell_sizes(t, phase)


def view_section():
    t = Tui(RUN, config=CONFIG).start()
    try:
        t.send(b"printf 'P%s\\n' A\r", 0.5)
        t.prefix(b"pv", 0.8)
        t.send(b"printf 'P%s\\n' B\r", 0.5)
        # A background tab, so the composed cells are not session-visible and
        # each pane's size is its cell's.
        t.send(b"\x1bt", 0.8)
        sm_compose_view(t, panes=(0, 1), settle=1.2)
        in_view = t.has("View 1")
        check("a view is displayed", in_view, "\n".join(t.rows_text()))
        if not in_view:
            return
        screen = "\n".join(t.rows_text())
        check("view: the cells show both panes", "PA" in screen and "PB" in screen, screen)

        # `grid = false`, so the View starts in bsp: two cells side by side.
        side = report_sizes(t, 1, b"\x1bl")
        check("view: both cells report a size side by side",
              len(side) == 2 and side[0][0] == side[1][0] and side[0][1] < side[1][1],
              side)
        if len(side) != 2:
            t.dump("view sizes before")
            return

        t.send(OPEN, 1.0)
        items = popup_rows(t)
        if VERBOSE:
            t.dump("switcher in a view")
        names = [i[2:] for i in items or []]
        check("view: the switcher lists the View's layouts, grid absent",
              names == EXPECTED, items if items is not None else "\n".join(t.rows_text()))
        check("view: the current layout is marked", "* bsp" in (items or []), items)

        pick(t, "rows", items or [])
        t.pump(0.8)
        check("view: the switcher closed", popup_rows(t) is None,
              "\n".join(t.rows_text()))
        check("view: still in the view", t.has("View 1"), "\n".join(t.rows_text()))

        # The focused cell is still the right/second one; in rows it is the
        # bottom one, so Alt+k reaches the other.
        stacked = report_sizes(t, 2, b"\x1bk")
        detail = f"before {side}\nafter {stacked}\n" + "\n".join(t.rows_text())
        if VERBOSE:
            print(f"        sizes before {side} after {stacked}")
        check("view: both cells report a size after the switch", len(stacked) == 2, detail)
        if len(stacked) == 2:
            (_, _, r_a, c_a), (_, _, r_b, c_b) = stacked
            (_, _, side_rows, side_cols), _ = side
            check("view: the cells are stacked full width",
                  c_a == c_b and c_a > side_cols * 1.5 and r_a < side_rows * 0.75
                  and r_b < side_rows * 0.75,
                  detail)

        t.prefix(b"wq", 1.2)
        check("view: left the view", not t.has("View 1"), "\n".join(t.rows_text()))
        check("view: the tab underneath kept its own layout",
              status_layout(t) == "bsp", t.rows_text()[-1])
        check("client is alive after the view checks", t.alive())

        server = t.log("server")
        check("view: the list and the switch went to the server as View messages",
              "ListViewLayouts" in server and "ViewSetLayout" in server,
              [ln for ln in server.splitlines() if "View" in ln][-10:])
        check("view: no ListLayouts or LayoutSet reached the server",
              "ListLayouts" not in server and "LayoutSet" not in server,
              [ln for ln in server.splitlines()
               if "ListLayouts" in ln or "LayoutSet" in ln][:10])
    finally:
        t.kill()
    check_logs(t, "view")


def check_logs(t, label):
    for which in ("client", "server"):
        body = t.log(which)
        check(f"{label}: no panic in {which}.log", "panicked at" not in body,
              body[-1500:])
        check(f"{label}: {which}.log is not empty", len(body) > 0)


def main():
    print(f"layout_selector (protocol {PROTOCOL_VERSION})")
    switcher_section()
    move_section()
    popup_section()
    view_section()

    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nall passed")


if __name__ == "__main__":
    main()
