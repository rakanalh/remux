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

Run: python3 tests/pty/popup_terminal.py
"""
import re, sys, time
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


def main():
    fails = []

    def check(ok, msg):
        print(("  PASS  " if ok else "  FAIL  ") + msg)
        if not ok:
            fails.append(msg)

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
    print(f"\n{'FAILED: ' + str(len(fails)) if fails else 'ALL PASS'}")
    for f in fails:
        print("  - " + f)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
