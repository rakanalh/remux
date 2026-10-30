#!/usr/bin/env python3
"""The layout switcher (Alt+Shift+L): list the active tab's layouts, apply one.

A real PTY, because the switcher is a CLIENT overlay opened by a key the client
decodes: the server only answers `ListLayouts` and applies `LayoutSet`.

What it covers:

  1  each byte sequence a terminal sends for Alt+Shift+L opens the switcher:
     legacy `ESC L`, and the CSI-u forms `CSI 108;4u` (base key + Shift) and
     `CSI 76;4u` (shifted key + Shift)
  2  the list is the SERVER's `[layouts]`: `grid = false` leaves grid out, the
     other five automatic layouts are listed, and the current one is marked
  3  Enter applies the highlighted layout: the status bar's layout name, which
     the SERVER composites, changes to it
  4  a `LayoutSet grid` bound in config is refused by the server, so the client
     never decides which layouts exist
  5  after a manual resize (which ejects the tab to Custom) and a switch away,
     `custom` is offered and restores
  6  the client is alive and neither log has a panic

Run from the repo root:
    python3 tests/pty/layout_selector.py [-v]
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import PROTOCOL_VERSION, Tui  # noqa: E402

RUN = "/tmp/rmx-lsel"
VERBOSE = "-v" in sys.argv

ENCODINGS = [
    ("ESC L", b"\x1bL"),
    ("CSI 108;4u", b"\x1b[108;4u"),
    ("CSI 76;4u", b"\x1b[76;4u"),
]
EXPECTED = ["bsp", "master", "monocle", "columns", "rows"]
TITLE = "Switch Layout"

CONFIG = """
[layouts]
grid = false

[keybindings.command]
"Alt-g" = "LayoutSet grid"
"Alt-y" = "LayoutSet columns"
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


def main():
    print(f"layout_selector (protocol {PROTOCOL_VERSION})")
    t = Tui(RUN, config=CONFIG).start()
    try:
        current = status_layout(t)
        check("starts in bsp", current == "bsp", t.rows_text()[-1])

        targets = ["monocle", "columns", "rows"]
        for (label, seq), target in zip(ENCODINGS, targets):
            t.send(seq, 1.0)
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

        t.send(b"\x1bg", 1.0)
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
        t.send(b"\x1bL", 1.0)
        items = popup_rows(t) or []
        names = [i[2:] for i in items]
        check("custom is listed and marked while the tab is custom",
              "* custom" in items, items)
        pick(t, "bsp", items)
        check("switching away from custom", status_layout(t) == "bsp",
              t.rows_text()[-1])
        t.send(b"\x1bL", 1.0)
        items = popup_rows(t) or []
        names = [i[2:] for i in items]
        check("the remembered custom layout is still offered",
              names == EXPECTED + ["custom"], names)
        pick(t, "custom", items)
        check("custom restores", status_layout(t) == "custom", t.rows_text()[-1])

        check("client is alive", t.alive())
    finally:
        t.kill()

    for which in ("client", "server"):
        body = t.log(which)
        check(f"no panic in {which}.log", "panicked at" not in body, body[-1500:])
        check(f"{which}.log is not empty", len(body) > 0)

    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nall passed")


if __name__ == "__main__":
    main()
