"""Popup terminal (real-PTY harness): the actual keybindings, through the real
client TUI.

The frame harness covers the server/render side. Only a real PTY can verify the
bindings, the terminal cursor position the client programs, and that the client
survives it all:

  1. `Alt-p` (\\x1bp) toggles the popup on -> a centered bordered box appears.
  2. The real terminal cursor lands INSIDE the popup interior.
  3. Typing goes into the popup.
  4. `Alt-p` again toggles it off -> back to the normal frame.
  5. `Ctrl-a p o` (the "pane -> popup" chord) toggles it too.
  6. The client stays alive throughout, and no panic lands in the logs.

The client places the popup itself, so a second group of cases runs with a
34-column left sidebar and checks the geometry the design fixes (see
docs/sessions/popup-positions-design.md): a float centred on the whole window
OVER the sidebar, the open-edge drawers, live config reload, the runtime resize
keys, and one popup per tab.

Run: python3 tests/pty/popup_terminal.py
"""
import os, re, sys, time
from pty_harness import Tui, PREFIX

RUNDIR = "/tmp/rmxpop/pty"
COLS, ROWS = 120, 40

ALT_P = b"\x1bp"


def expected_popup_rect(cols, rows, wpct=80, hpct=80):
    """Mirror of `popup_rect` in src/server/layout.rs (content area = rows-1)."""
    aw, ah = cols, rows - 1
    w = min(max(aw * wpct // 100, 12), aw)
    h = min(max(ah * hpct // 100, 5), ah)
    return {"x": (aw - w) // 2, "y": (ah - h) // 2, "width": w, "height": h}


def boxes_in(rows_text, cols):
    """Every bordered box whose four rounded corners are present."""
    found = []
    for y, row in enumerate(rows_text):
        for x, ch in enumerate(row):
            if ch != "╭":  # ╭
                continue
            for x2 in range(x + 1, min(len(row), cols)):
                if row[x2] != "╮":  # ╮
                    continue
                w = x2 - x + 1
                for y2 in range(y + 1, len(rows_text)):
                    r2 = rows_text[y2]
                    if (len(r2) > x2 and r2[x] == "╰" and r2[x2] == "╯"):
                        found.append({"x": x, "y": y, "width": w,
                                      "height": y2 - y + 1})
                        break
                break
    return found


def make_check(fails):
    def check(ok, msg):
        print(("  PASS  " if ok else "  FAIL  ") + msg)
        if not ok:
            fails.append(msg)
    return check


def no_sidebar_case():
    fails = []
    check = make_check(fails)

    tui = Tui(RUNDIR, cols=COLS, rows=ROWS)
    tui.start()
    tui.pump(1.6)
    tui.send(b"echo BASE_PANE_MARK\r")
    tui.pump(1.0)

    rows_before = list(tui.rows_text())
    boxes_before = boxes_in(rows_before, COLS)
    check(any("BASE_PANE_MARK" in r for r in rows_before),
          "baseline: the client is up and the shell echoes")
    print(f"        boxes before: {boxes_before}")

    exp = expected_popup_rect(COLS, ROWS)
    print(f"        expected popup rect: {exp}")

    # -- 1/2/3: Alt-p opens the popup; cursor inside; typing lands there. ---
    print("\n[1] Alt-p opens the popup")
    tui.send(ALT_P)
    tui.pump(1.6)
    rows_open = list(tui.rows_text())
    boxes_open = boxes_in(rows_open, COLS)
    print(f"        boxes open:   {boxes_open}")
    check(exp in boxes_open, f"a bordered box appeared at {exp}")
    check(any("popup" in r for r in rows_open), "the popup's title is drawn")

    print("\n[2] the terminal cursor lands inside the popup interior")
    cx, cy = tui.screen.cursor.x, tui.screen.cursor.y
    inside = (exp["x"] < cx < exp["x"] + exp["width"] - 1
              and exp["y"] < cy < exp["y"] + exp["height"] - 1)
    check(inside, f"cursor ({cx},{cy}) is inside the popup interior "
                  f"(x {exp['x']+1}..{exp['x']+exp['width']-2}, "
                  f"y {exp['y']+1}..{exp['y']+exp['height']-2})")

    print("\n[3] typing goes into the popup")
    tui.send(b"echo PTY_POPUP_MARK\r")
    tui.pump(1.4)
    rows_typed = list(tui.rows_text())
    hit_rows = [y for y, r in enumerate(rows_typed) if "PTY_POPUP_MARK" in r]
    check(bool(hit_rows), "the marker was echoed somewhere on screen")
    in_popup = all(exp["y"] < y < exp["y"] + exp["height"] - 1 for y in hit_rows)
    check(in_popup, f"every occurrence is on a popup interior row (rows {hit_rows})")
    hit_cols = []
    for y in hit_rows:
        m = re.search("PTY_POPUP_MARK", rows_typed[y])
        if m:
            hit_cols.append((m.start(), m.end()))
    in_cols = all(exp["x"] < s and e <= exp["x"] + exp["width"] - 1
                  for s, e in hit_cols)
    check(in_cols, f"and within the popup's columns (spans {hit_cols})")
    check(tui.alive(), "the client is alive after typing into the popup")

    # -- 4: Alt-p closes it. -----------------------------------------------
    print("\n[4] Alt-p closes the popup")
    tui.send(ALT_P)
    tui.pump(1.6)
    rows_closed = list(tui.rows_text())
    boxes_closed = boxes_in(rows_closed, COLS)
    check(exp not in boxes_closed, f"the popup box is gone (boxes {boxes_closed})")
    check(boxes_closed == boxes_before,
          f"back to the original frame: {boxes_closed} == {boxes_before}")
    check(not any("PTY_POPUP_MARK" in r for r in rows_closed),
          "no popup content lingers on the frame")
    check(any("BASE_PANE_MARK" in r for r in rows_closed),
          "the underlying pane's content is still there")

    # Input after closing must reach the real pane again.
    tui.send(b"echo AFTER_POPUP_MARK\r")
    tui.pump(1.4)
    check(any("AFTER_POPUP_MARK" in r for r in tui.rows_text()),
          "input after toggle-off reaches the real pane again")

    # -- 5: the Ctrl-a p o chord. ------------------------------------------
    print("\n[5] the Ctrl-a p o chord toggles the popup")
    tui.send(PREFIX)
    tui.pump(0.5)
    tui.send(b"p")
    tui.pump(0.5)
    tui.send(b"o")
    tui.pump(1.8)
    boxes_chord = boxes_in(list(tui.rows_text()), COLS)
    check(exp in boxes_chord, f"Ctrl-a p o opened the popup (boxes {boxes_chord})")
    tui.send(b"echo CHORD_POPUP_MARK\r")
    tui.pump(1.4)
    rows_chord = list(tui.rows_text())
    chord_rows = [y for y, r in enumerate(rows_chord) if "CHORD_POPUP_MARK" in r]
    check(bool(chord_rows) and all(
        exp["y"] < y < exp["y"] + exp["height"] - 1 for y in chord_rows),
        f"typing after the chord goes into the popup (rows {chord_rows})")

    tui.send(PREFIX)
    tui.pump(0.4)
    tui.send(b"p")
    tui.pump(0.4)
    tui.send(b"o")
    tui.pump(1.8)
    check(exp not in boxes_in(list(tui.rows_text()), COLS),
          "the chord closes it again")

    # -- 6: alive + no panic. ----------------------------------------------
    print("\n[6] the client survived and nothing panicked")
    check(tui.alive(), "the client process is still alive")

    print("\n" + "\n".join(tui.rows_text()))

    logs = tui.log("client") + tui.log("server")
    check("panic" not in logs.lower(), "no panic in client.log / server.log")

    tui.kill()
    return fails


# ---------------------------------------------------------------------------
# With a left sidebar: the client places the popup
# ---------------------------------------------------------------------------

SB_COLS, SB_ROWS = 160, 48
SIDEBAR_W = 34
SIDEBAR = """
[[sidebar]]
edge = "left"
size = 34
visible = true
  [[sidebar.panel]]
  plugin = "agents"
  [[sidebar.panel]]
  plugin = "sessions"
"""


def cfg(position=None, wpct=None, hpct=None):
    """The sidebar config, with the popup keys when given."""
    keys = []
    if position:
        keys.append(f'popup_position = "{position}"')
    if wpct:
        keys.append(f"popup_width_pct = {wpct}")
    if hpct:
        keys.append(f"popup_height_pct = {hpct}")
    head = "[appearance]\n" + "\n".join(keys) + "\n" if keys else ""
    return head + SIDEBAR


def start(rundir, config):
    tui = Tui(rundir, cols=SB_COLS, rows=SB_ROWS, config=config)
    tui.start()
    tui.pump(1.6)
    return tui


def write_config(tui, body):
    with open(f"{tui.rundir}/config/remux/config.toml", "w") as fh:
        fh.write(body)


def wait_for_reload(tui, baseline, timeout=12.0):
    """Wait for the client to say it reloaded, rather than sleeping a fixed
    time, so a slow save does not turn into a vacuous pass."""
    end = time.time() + timeout
    while time.time() < end:
        tui.pump(0.3)
        if tui.log("client").count("client: config reloaded") > baseline:
            tui.pump(1.0)
            return True
    return False


def corner_cols(row, glyph):
    return [x for x, ch in enumerate(row) if ch == glyph]


def float_over_sidebar_case():
    print("\n== float over a left sidebar ==")
    fails = []
    check = make_check(fails)
    tui = start("/tmp/rmxpop/sb_float", cfg())
    try:
        before = list(tui.rows_text())
        check(before[0][0] == "\u256d" and before[5][SIDEBAR_W - 1] == "\u2502",
              "the sidebar is framed at the left")
        tui.send(ALT_P)
        tui.pump(1.8)
        rows = list(tui.rows_text())
        # Box (16, 5, 128, 37), not (47, ...): it is centred on the WHOLE
        # window, over the sidebar.
        check(rows[5][16] == "\u256d" and rows[5][143] == "\u256e",
              f"top corners at x=16 and x=143 (row 5: {rows[5][10:20]!r}...)")
        check(rows[41][16] == "\u2570" and rows[41][143] == "\u256f",
              "bottom corners at y=41")
        check(16 < SIDEBAR_W, "the box overlaps the sidebar's columns")
        check(all(rows[y][:16] == before[y][:16] for y in range(SB_ROWS)),
              "sidebar text in columns 0..16 stays visible while the popup is up")
        check(any("popup" in r for r in rows), "the popup's title is drawn")
        cx, cy = tui.screen.cursor.x, tui.screen.cursor.y
        check(17 <= cx <= 142 and 6 <= cy <= 40,
              f"the cursor ({cx},{cy}) is inside the popup interior")
        tui.send(b"echo SB_FLOAT_MARK\r")
        tui.pump(1.2)
        rows = list(tui.rows_text())
        hits = [(y, r.index("SB_FLOAT_MARK")) for y, r in enumerate(rows)
                if "SB_FLOAT_MARK" in r]
        check(bool(hits) and all(6 <= y <= 40 and x >= 17 for y, x in hits),
              f"typed text lands in the box ({hits})")
        tui.send(ALT_P)
        tui.pump(1.8)
        after = list(tui.rows_text())
        check(all(after[y][:SIDEBAR_W] == before[y][:SIDEBAR_W]
                  for y in range(SB_ROWS)),
              "the sidebar is restored after the popup closes")
        check(after[5][16] != "\u256d", "the box is gone")
        check(tui.alive(), "the client is alive")
        logs = tui.log("client") + tui.log("server")
        check("panic" not in logs.lower(), "no panic in the logs")
    finally:
        tui.kill()
    return fails


def left_case():
    print("\n== popup_position = left ==")
    fails = []
    check = make_check(fails)
    tui = start("/tmp/rmxpop/sb_left", cfg("left"))
    try:
        tui.send(ALT_P)
        tui.pump(1.8)
        rows = list(tui.rows_text())
        # Box (34, 0, 100, 47): right of the sidebar, no left border column.
        check(rows[0][133] == "\u256e" and rows[46][133] == "\u256f",
              f"right corners at x=133 (row 0: {rows[0][128:137]!r})")
        check(all(rows[y][133] == "\u2502" for y in range(1, 46)),
              "the right border runs down the box")
        # The title chip sits at the start of the top edge, so the rule's end
        # is read on the bottom row.
        check(rows[46][SIDEBAR_W] == "\u2500" and rows[0][SIDEBAR_W] != "\u256d",
              "the edges run to the box boundary with no corner")
        check(all(rows[y][SIDEBAR_W] != "\u2502" for y in range(1, 46)),
              "no left border column")
        check("popup" in rows[0], "the title is on the top row")
        check(rows[0][SIDEBAR_W - 1] == "\u256e" or rows[0][SIDEBAR_W - 1] == "\u2502"
              or rows[1][SIDEBAR_W - 1] == "\u2502",
              "the sidebar frame is intact beside it")
        cx, cy = tui.screen.cursor.x, tui.screen.cursor.y
        check(SIDEBAR_W <= cx <= 132 and 1 <= cy <= 45,
              f"the cursor ({cx},{cy}) is inside the popup interior")
        check(tui.alive(), "the client is alive")
    finally:
        tui.kill()
    return fails


def top_case():
    print("\n== popup_position = top ==")
    fails = []
    check = make_check(fails)
    tui = start("/tmp/rmxpop/sb_top", cfg("top"))
    try:
        tui.send(ALT_P)
        tui.pump(1.8)
        rows = list(tui.rows_text())
        # Box (0, 0, 160, 37): covers the sidebar's columns, open at the top.
        check(rows[0][0] == "\u2502" and rows[0][159] == "\u2502",
              f"row 0 starts with the side borders, not a corner ({rows[0][:3]!r})")
        check("\u256d" not in rows[0] and "\u256e" not in rows[0],
              "no top border row")
        check(rows[36][0] == "\u2570" and rows[36][159] == "\u256f",
              "the bottom corners close the box at y=36")
        check("popup" in rows[36], "the title is on the bottom row")
        check(rows[37][0] == "\u256d" or rows[37][SIDEBAR_W - 1] == "\u2502",
              "the sidebar shows again below the box")
        cx, cy = tui.screen.cursor.x, tui.screen.cursor.y
        check(1 <= cx <= 158 and 0 <= cy <= 35,
              f"the cursor ({cx},{cy}) is inside the popup interior")
        check(tui.alive(), "the client is alive")
    finally:
        tui.kill()
    return fails


def top_right_corner(row, start=0):
    for x in range(start, len(row)):
        if row[x] == "\u256e":
            return x
    return -1


def live_reload_case():
    print("\n== live config reload moves an open popup ==")
    fails = []
    check = make_check(fails)
    tui = start("/tmp/rmxpop/sb_reload", cfg())
    try:
        tui.send(ALT_P)
        tui.pump(1.8)
        check(list(tui.rows_text())[5][16] == "\u256d", "the float is open")
        baseline = tui.log("client").count("client: config reloaded")
        write_config(tui, cfg("left", wpct=50))
        check(wait_for_reload(tui, baseline), "the client logged the reload")
        rows = list(tui.rows_text())
        # Left, 50% of the 126-column content: box (34, 0, 63, 47).
        check(rows[0][96] == "\u256e" and rows[46][96] == "\u256f",
              f"the box moved to the left drawer (row 0: {rows[0][90:100]!r})")
        check(rows[5][16] != "\u256d", "the old float is gone")
        check(rows[46][SIDEBAR_W] == "\u2500", "and it is open on the left")
        check(tui.alive(), "the client is alive")
    finally:
        tui.kill()
    return fails


def runtime_resize_case():
    print("\n== the runtime resize keys widen a left drawer ==")
    fails = []
    check = make_check(fails)
    tui = start("/tmp/rmxpop/sb_resize", cfg("left", wpct=50))
    try:
        tui.send(ALT_P)
        tui.pump(1.8)
        before = top_right_corner(list(tui.rows_text())[0], SIDEBAR_W)
        check(before == 96, f"the drawer is 63 wide before ({before})")
        # Ctrl-a p R l: the sticky resize group, one step to the right.
        tui.prefix(b"pR", 0.8)
        tui.send(b"l", 1.2)
        tui.send(b"\x1b", 1.0)
        after = top_right_corner(list(tui.rows_text())[0], SIDEBAR_W)
        check(after > before, f"the drawer is wider after ({before} -> {after})")
        check(tui.alive(), "the client is alive")
    finally:
        tui.kill()
    return fails


def per_tab_case():
    print("\n== one popup per tab ==")
    fails = []
    check = make_check(fails)
    marker_dir = "/tmp/rmxpop/tab2dir"
    os.makedirs(marker_dir, exist_ok=True)
    tui = start("/tmp/rmxpop/sb_tabs", cfg())
    try:
        # Tab 1: open the popup and type a marker.
        tui.send(ALT_P)
        tui.pump(1.8)
        tui.send(b"echo TAB1_POPUP_MARK\r")
        tui.pump(1.2)
        check(tui.has("TAB1_POPUP_MARK"), "tab 1's popup shows its marker")

        # Tab 2: a new tab does not show tab 1's popup.
        tui.prefix(b"tn", 1.8)
        check(not tui.has("TAB1_POPUP_MARK"),
              "tab 2 does not show tab 1's popup")
        tui.send(f"cd {marker_dir}\r".encode())
        tui.pump(1.0)
        tui.send(ALT_P)
        tui.pump(1.8)
        check(list(tui.rows_text())[5][16] == "\u256d", "tab 2's own popup opens")
        tui.send(b"pwd\r")
        tui.pump(1.2)
        rows = list(tui.rows_text())
        # A path on its own line inside the popup, not the typed command or a
        # prompt (which carries the `$`).
        check(any("/tmp/rmxpop/tab2dir" in r[17:143] and "$" not in r[17:143]
                  for r in rows[6:41]),
              "the popup starts in the directory tab 2's pane is in")

        # Back to tab 1: its popup is still up, with its marker.
        tui.prefix(b"t1", 1.8)
        check(tui.has("TAB1_POPUP_MARK"),
              "back on tab 1 its popup is still open with its marker")
        check(list(tui.rows_text())[5][16] == "\u256d", "and it is drawn as a box")

        # Close tab 2: the client survives.
        tui.prefix(b"t2", 1.8)
        tui.prefix(b"tx", 1.8)
        check(tui.alive(), "the client survives closing a tab with a popup")
        logs = tui.log("client") + tui.log("server")
        check("panic" not in logs.lower(), "no panic in the logs")
        tui.prefix(b"t1", 1.5)
        check(tui.alive(), "and still after returning to tab 1")
    finally:
        tui.kill()
    return fails


def main():
    all_fails = []
    for case in (no_sidebar_case, float_over_sidebar_case, left_case, top_case,
                 live_reload_case, runtime_resize_case, per_tab_case):
        all_fails += [f"{case.__name__}: {f}" for f in case()]
    print(f"\n{'FAILED: ' + str(len(all_fails)) if all_fails else 'ALL PASS'}")
    for f in all_fails:
        print("  - " + f)
    return 1 if all_fails else 0


if __name__ == "__main__":
    sys.exit(main())
