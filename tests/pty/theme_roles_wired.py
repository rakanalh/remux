#!/usr/bin/env python3
"""Every documented `[appearance.theme]` role must actually reach the screen.

`frame_bg`, `pane_label_fg` and `pane_label_bg` were declared in all three theme
structs and documented in `config.sample.toml`, but a whole-tree grep found NO
consumer -- every border cell hardcoded `bg: CellColor::Default` and the label
was drawn in the border color. A user set them and nothing happened, silently.
`tab_inactive_bg` and `layout_indicator_fg`/`_bg` are the named roles that
replaced the magic `Indexed(237)` / `Indexed(0)+Indexed(245)` literals.

Two modes, and BOTH matter:

  themed   a config.toml sets each role to a distinctive hex color; the test
           asserts that color really appears at the cells it names.
  default  no config at all; the test asserts the SHIPPED DEFAULTS still render
           exactly as they did before the roles were wired up -- borders on the
           terminal's default background, the label in the border color, the
           layout indicator black-on-grey. Wiring a role up must not change
           anyone's existing look unless they opted in.

Run from the repo root:
    python3 tests/pty/theme_roles_wired.py themed
    python3 tests/pty/theme_roles_wired.py default
    python3 tests/pty/theme_roles_wired.py          # both
"""
import sys
from pty_harness import Tui

FRAME_BG = "123456"
FRAME_FG = "abcdef"
LABEL_FG = "fedcba"
LABEL_BG = "654321"
TAB_INACTIVE_BG = "0fa0fa"
TAB_INACTIVE_FG = "a0fa0f"
IND_FG = "ff00ff"
IND_BG = "00ff00"

CONFIG = f"""
[appearance.theme]
frame_fg = "#{FRAME_FG}"
frame_bg = "#{FRAME_BG}"
pane_label_fg = "#{LABEL_FG}"
pane_label_bg = "#{LABEL_BG}"
tab_inactive_fg = "#{TAB_INACTIVE_FG}"
tab_inactive_bg = "#{TAB_INACTIVE_BG}"
layout_indicator_fg = "#{IND_FG}"
layout_indicator_bg = "#{IND_BG}"
"""

BOX_EDGES = {"─", "│"}  # ─ │
BOX_CORNERS = {"╭", "╮", "╰", "╯"}  # ╭ ╮ ╰ ╯


def style(cell):
    return (str(cell.fg), str(cell.bg), bool(cell.bold))


def border_styles(t):
    """Style triples of every plain box edge/corner cell (labels excluded)."""
    out = set()
    for y in range(t.screen.lines - 1):
        row = t.screen.buffer[y]
        for x in range(t.screen.columns):
            if row[x].data in BOX_EDGES or row[x].data in BOX_CORNERS:
                out.add(style(row[x]))
    return out


def label_cells(t, text):
    """Style triples of the characters of `text` where it appears as a
    top-border label (i.e. on a row that starts a box, right after `╭ `)."""
    out = set()
    for y in range(t.screen.lines - 1):
        line = "".join(t.screen.buffer[y][x].data for x in range(t.screen.columns))
        start = 0
        while True:
            i = line.find("╭ " + text, start)
            if i < 0:
                break
            for x in range(i + 2, i + 2 + len(text)):
                out.add(style(t.screen.buffer[y][x]))
            start = i + 1
    return out


def right_segment(t):
    row = t.screen.buffer[t.screen.lines - 1]
    last = t.screen.columns - 1
    want = style(row[last])
    x = last
    while x >= 0 and style(row[x]) == want:
        x -= 1
    return "".join(row[c].data for c in range(x + 1, last + 1)), want


def stack_tab_styles(t):
    """Style triples of the two stacked-pane tab labels in the top border.

    After `Prefix p a` the top border carries two equal-width tab blocks; the
    ACTIVE one is mode-colored and the inactive one is the block whose role is
    `tab_inactive_fg`/`tab_inactive_bg`. Returns every distinct style found on
    the border row's non-box cells.
    """
    out = set()
    for y in range(t.screen.lines - 1):
        row = t.screen.buffer[y]
        line = "".join(row[x].data for x in range(t.screen.columns))
        if "╭" not in line:
            continue
        for x in range(t.screen.columns):
            if row[x].data not in BOX_EDGES and row[x].data not in BOX_CORNERS:
                out.add(style(row[x]))
    return out


def run(mode):
    print(f"===== mode: {mode} =====")
    cfg = CONFIG if mode == "themed" else None
    t = Tui(f"/tmp/rmxthm{mode[:3]}", cols=120, rows=40, config=cfg).start()
    fails = []
    try:
        t.send("clear\r", 0.5)
        t.send("printf 'THEME_MARKER\\n'\r", 0.6)
        if not t.has("THEME_MARKER"):
            print("ABORT: no shell output; the client never came up")
            t.dump("no shell")
            t.kill()
            sys.exit(1)

        borders = border_styles(t)
        labels = label_cells(t, "sh")
        seg_text, seg_style = right_segment(t)
        print(f"border styles : {sorted(borders)}")
        print(f"label 'sh'    : {sorted(labels)}")
        print(f"right segment : {seg_text!r} {seg_style}")

        if not borders:
            fails.append("no box-border cells on screen at all")
        if not labels:
            fails.append("no top-border 'sh' pane label found")

        # A stacked pane, so the inactive-tab block is on screen.
        t.prefix(b"pa", 1.2)
        t.send("clear\r", 0.5)
        stack_styles = stack_tab_styles(t)
        print(f"stack tabs    : {sorted(stack_styles)}")

        if mode == "themed":
            bad = [s for s in borders if s[1] != FRAME_BG]
            if bad:
                fails.append(f"frame_bg is not on the border cells: got bg {bad}, "
                             f"want #{FRAME_BG} everywhere")
            if labels != {(LABEL_FG, LABEL_BG, False)}:
                fails.append(f"pane_label_fg/bg not applied to the label: "
                             f"{sorted(labels)}, want {(LABEL_FG, LABEL_BG, False)}")
            if seg_style != (IND_FG, IND_BG, False):
                fails.append(f"layout_indicator_fg/bg not applied: {seg_style}, "
                             f"want {(IND_FG, IND_BG, False)}")
            if (TAB_INACTIVE_FG, TAB_INACTIVE_BG, False) not in stack_styles:
                fails.append(f"tab_inactive_bg not applied to the inactive stack "
                             f"tab: {sorted(stack_styles)}, want "
                             f"{(TAB_INACTIVE_FG, TAB_INACTIVE_BG, False)}")
        else:
            # Shipped defaults: byte-for-byte the pre-wiring appearance.
            bad = [s for s in borders if s[1] != "default"]
            if bad:
                fails.append(f"DEFAULT border background changed: {bad} "
                             "(was the terminal default everywhere)")
            # The label used to be drawn in the border color on the default bg.
            border_fgs = {s[0] for s in borders}
            for lab in labels:
                if lab[1] != "default":
                    fails.append(f"DEFAULT pane label background changed: {lab} "
                                 "(was the terminal default)")
                if lab[0] not in border_fgs:
                    fails.append(f"DEFAULT pane label fg {lab[0]} is not a border "
                                 f"color {sorted(border_fgs)} (it used to be)")
            if seg_style != ("000000", "8a8a8a", False):
                fails.append(f"DEFAULT layout indicator changed: {seg_style}, "
                             "was black on grey-245 ('000000','8a8a8a',False)")
            if ("9399b2", "3a3a3a", False) not in stack_styles:
                fails.append(f"DEFAULT inactive stack tab changed: "
                             f"{sorted(stack_styles)}, want tab_inactive_fg "
                             "#9399b2 on Indexed(237)=#3a3a3a")

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
            print(f"RESULT: FAIL ({mode})")
            return 1
        print(f"RESULT: PASS ({mode})")
        return 0
    finally:
        t.kill()


def main():
    modes = [sys.argv[1]] if len(sys.argv) > 1 else ["default", "themed"]
    rc = 0
    for m in modes:
        rc |= run(m)
    sys.exit(rc)


if __name__ == "__main__":
    main()
