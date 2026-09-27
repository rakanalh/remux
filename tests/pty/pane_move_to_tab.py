"""`Prefix p t`: the tab picker that moves the focused pane to another tab.

A real PTY, because the picker is a CLIENT overlay: the server only supplies the
session tree it lists and performs the move the picker asks for.

What it covers:

  1  which-key under `Prefix p` lists `t move to tab`
  2  with one tab holding one pane there is nothing to move to: the picker
     closes itself, nothing is sent, and the next keystroke reaches the shell
  3  with three tabs, opened from Tab 1: the popup is titled `Move pane to tab`,
     lists `+ new tab` and the OTHER tabs as `<n>: <name>`, and never the
     source tab; the highlight starts on the first other tab
  4  Esc closes it and changes nothing
  5  j + Enter moves the pane into Tab 3; the emptied Tab 1 disappears; focus
     follows the pane, proven by typing a pane-assembled marker AFTER the move
     and finding it below the pane's pre-move output in the same column span
  6  k + Enter on `+ new tab` breaks the pane out into a tab of its own
  7  with a View displayed the picker does not open
  8  the client stays alive and neither log holds a panic

Markers are ASSEMBLED by the shell (`printf 'RAN:%s' b` prints `RAN:b`), so the
typed command line can never satisfy the search.

Run from the repo root: python3 tests/pty/pane_move_to_tab.py [-v]
"""
import re
import sys

from pty_harness import Tui, sm_compose_view

RUNDIR = "/tmp/rmxpmtp"
COLS, ROWS = 100, 30
ESC = b"\x1b"
TITLE = "Move pane to tab"
VERBOSE = "-v" in sys.argv

fails = []


def check(cond, msg):
    print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def picker(t):
    """The picker's rows as [(text, selected)], or None when it is not drawn.

    The highlighted row is the one whose background differs from the popup's
    own border cell on that row. Comparing rows with each other instead would
    tie on a two-row list and pick one at random.
    """
    rows = t.rows_text()
    top = next((y for y, r in enumerate(rows) if TITLE in r and "╭" in r), None)
    if top is None:
        return None
    x0 = rows[top].index("╭")
    x1 = rows[top].rindex("╮")
    out = []
    y = top + 1
    while y < len(rows) and rows[y][x0] == "│":
        line = t.screen.buffer[y]
        out.append((rows[y][x0 + 1:x1].strip(), line[x0 + 2].bg != line[x0].bg))
        y += 1
    return out


def selected(rows):
    hits = [text for text, sel in rows or [] if sel]
    return hits[0] if len(hits) == 1 else None


def tab_names(t):
    """Tab names on the status bar, left to right."""
    bar = t.rows_text()[-1]
    return re.findall(r"Tab \d+", bar)


def spans(t, needle):
    """(row, start, end) of every occurrence. `finditer`: panes share rows."""
    return [(y, m.start(), m.end())
            for y, r in enumerate(t.rows_text()) for m in re.finditer(re.escape(needle), r)]


def pane_columns(t, row, col):
    """The column range [left, right) of the bordered pane holding (row, col)."""
    line = t.rows_text()[row]
    left = max(i for i in range(col) if line[i] == "│") + 1
    right = min(i for i in range(col, len(line)) if line[i] == "│")
    return left, right


def open_picker(t):
    t.prefix(b"pt", 1.2)
    return picker(t)


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    try:
        t.pump(1.5)

        print("[1] which-key lists the binding")
        t.prefix(b"p", 0.6)
        check(any(re.search(r"\bt move to tab\b", r) for r in t.rows_text()),
              "`Prefix p` shows `t move to tab`")
        t.send(ESC, 0.5)

        print("[2] one tab holding one pane: nothing to offer")
        rows = open_picker(t)
        check(rows is None, f"the picker is not on screen ({rows})")
        check("tab picker: closed, no other tab" in t.log("client"),
              "the client read the tree and found nothing to offer")
        t.send(b"printf 'N:%s\\n' ok\r", 0.8)
        check(bool(spans(t, "N:ok")), "the next keystroke reaches the shell")
        check("PaneMoveToTabTarget" not in t.log("server"), "no move was sent")

        print("[3] the listing, opened from Tab 1 of three")
        t.send(b"clear; printf 'RAN:%s\\n' a\r", 0.8)
        t.prefix(b"tn", 1.0)
        t.send(b"printf 'IN:%s\\n' two\r", 0.6)
        t.prefix(b"tn", 1.0)
        t.send(b"printf 'IN:%s\\n' three\r", 0.6)
        t.prefix(b"t1", 1.0)
        check(bool(spans(t, "RAN:a")), "back on Tab 1")
        rows = open_picker(t)
        if VERBOSE:
            t.dump("picker")
        texts = [text for text, _ in rows or []]
        check(rows is not None, "the picker opens")
        check(texts == ["+ new tab", "2: Tab 2", "3: Tab 3"],
              f"`+ new tab` then the other tabs, and not Tab 1 ({texts})")
        check(selected(rows) == "2: Tab 2",
              f"the highlight starts on the first other tab ({selected(rows)})")

        print("[4] Esc changes nothing")
        t.send(ESC, 0.8)
        check(picker(t) is None, "the picker closed")
        check(tab_names(t) == ["Tab 1", "Tab 2", "Tab 3"], f"tabs unchanged ({tab_names(t)})")
        check(bool(spans(t, "RAN:a")), "still on Tab 1")
        check("PaneMoveToTabTarget" not in t.log("server"), "no move was sent")

        print("[5] j + Enter moves the pane into Tab 3")
        open_picker(t)
        t.send(b"j", 0.4)
        check(selected(picker(t)) == "3: Tab 3", f"j highlights Tab 3 ({selected(picker(t))})")
        t.send(b"\r", 1.2)
        check(picker(t) is None, "the picker closed")
        check(tab_names(t) == ["Tab 2", "Tab 3"],
              f"the emptied Tab 1 is gone ({tab_names(t)})")
        t.send(b"printf 'RAN:%s\\n' b\r", 1.0)
        if VERBOSE:
            t.dump("after move")
        before, after = spans(t, "RAN:a"), spans(t, "RAN:b")
        check(len(before) == 1 and len(after) == 1, f"both markers visible ({before}, {after})")
        if len(before) == 1 and len(after) == 1:
            same_pane = pane_columns(t, *before[0][:2]) == pane_columns(t, *after[0][:2])
            check(same_pane and before[0][0] < after[0][0],
                  "typing lands in the moved pane, below its pre-move output")
        check(bool(spans(t, "IN:three")), "Tab 3's own pane is beside it")
        check(not spans(t, "IN:two"), "Tab 2 is not the one on screen")

        print("[6] k + Enter on `+ new tab` breaks the pane out")
        rows = open_picker(t)
        texts = [text for text, _ in rows or []]
        check(texts == ["+ new tab", "1: Tab 2"], f"listing from Tab 3 ({texts})")
        t.send(b"k", 0.4)
        check(selected(picker(t)) == "+ new tab", f"k highlights `+ new tab` ({selected(picker(t))})")
        t.send(b"\r", 1.2)
        check(tab_names(t) == ["Tab 2", "Tab 3", "Tab 3"],
              f"a new last tab, named as TabNew names it ({tab_names(t)})")
        t.send(b"printf 'RAN:%s\\n' c\r", 1.0)
        check(bool(spans(t, "RAN:b")) and bool(spans(t, "RAN:c")),
              "the moved pane keeps its output and takes the next keystroke")
        check(not spans(t, "IN:three"), "it is alone in its new tab")

        print("[7] a displayed View has no tab to move out of")
        sm_compose_view(t, tab="Tab 2", panes=(0,), settle=1.2)
        check(t.has("View 1"), "a View is on screen")
        tabs_before = tab_names(t)
        rows = open_picker(t)
        check(rows is None, f"the picker does not open over the View ({rows})")
        check("tab picker: not opened, a view is displayed" in t.log("client"),
              "the client refused it because a View is displayed")
        check(t.has("View 1"), "the View is still on screen")
        check(tab_names(t) == tabs_before, f"tabs unchanged ({tab_names(t)})")

        print("[8] health")
        check(t.alive(), "the client is alive")
    except Exception as e:
        check(False, f"the harness could not continue: {e!r}")
    finally:
        t.kill()

    server = t.log("server")
    check("PaneMoveToTabTarget { tab_id: Some(" in server
          and "PaneMoveToTabTarget { tab_id: None }" in server,
          "the server received both moves")
    for which in ("client", "server"):
        check("panicked at" not in t.log(which), f"no panic in {which}.log")

    if fails:
        print(f"\nFAIL: {len(fails)} check(s) failed")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
