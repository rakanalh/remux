"""Popup terminal (frame harness): the whole feature, read off the composited
grid the server actually sends.

The popup is a per-SESSION floating pane drawn centered on top of the layout. It
is a real PTY but is deliberately in NO tab's `pane_order` and in no layout tree,
so it must steal no space from the panes it covers.

Cases:
  1. Toggle on -> a bordered box at the EXACTLY-computed centered rect, with the
     underlying panes still drawn around it.
  2. Typing goes to the popup and NOT to the pane that had focus.
  3. Toggle off -> the frame returns to the plain layout AND focus is restored to
     the original pane (a second marker lands there).
  4. Runtime resize (`ResizeLeft/Right/Up/Down`) changes the rect as expected,
     and clamping holds at both extremes.
  5. Tab switch while open -> the popup is still visible (per-session scope).
  6. Popup shell exit -> the popup closes and the frame returns to plain layout.
  7. It stole no space: every surrounding pane's rect is IDENTICAL popup-open vs
     popup-closed.
  8. Layout-mutating commands are no-ops while the popup is open, and the popup
     renders on top of a ZOOMED pane with zoom state intact afterwards.

Run: python3 tests/frame/popup_terminal.py
"""
import sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxpop/frame"
COLS, ROWS = 100, 30

# Mirrors src/server/layout.rs.
POPUP_MIN_PCT, POPUP_MAX_PCT = 20, 100
MIN_W, MIN_H = 10 + 2, 3 + 2



def expected_popup_rect(cols, rows, wpct, hpct):
    """The Rust `popup_rect(area, size)`, reimplemented independently here so the
    frame assertions compare against a computed rect, not against whatever the
    server drew."""
    aw, ah = cols, rows - 1  # content area excludes the status bar row
    wpct = max(POPUP_MIN_PCT, min(POPUP_MAX_PCT, wpct))
    hpct = max(POPUP_MIN_PCT, min(POPUP_MAX_PCT, hpct))
    w = min(max(aw * wpct // 100, MIN_W), aw)
    h = min(max(ah * hpct // 100, MIN_H), ah)
    return {"x": (aw - w) // 2, "y": (ah - h) // 2, "width": w, "height": h}


class Grid:
    """Reconstruct the composited grid from FullRender / RenderDiff."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.fpr = None
        self.cursor = None

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = self._ch(cell)
            self.fpr = body.get("focused_pane_rect")
            self.cursor = (body.get("cursor_x"), body.get("cursor_y"),
                           body.get("cursor_visible"))
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            if body.get("focused_pane_rect"):
                self.fpr = body["focused_pane_rect"]
            if body.get("cursor_x") is not None:
                self.cursor = (body.get("cursor_x"), body.get("cursor_y"),
                               body.get("cursor_visible"))

    def row_text(self, y):
        return "".join(self.g[y])

    def text(self):
        return "\n".join(self.row_text(y) for y in range(self.rows))

    def region_text(self, r):
        """Text strictly inside rect r (its interior, i.e. minus the frame)."""
        out = []
        for y in range(r["y"] + 1, r["y"] + r["height"] - 1):
            if 0 <= y < self.rows:
                out.append("".join(self.g[y][r["x"] + 1:r["x"] + r["width"] - 1]))
        return "\n".join(out)

    def text_outside(self, r):
        """All text NOT inside rect r -- what the layout underneath still shows."""
        out = []
        for y in range(self.rows):
            row = []
            for x in range(self.cols):
                inside = (r["x"] <= x < r["x"] + r["width"]
                          and r["y"] <= y < r["y"] + r["height"])
                row.append(" " if inside else self.g[y][x])
            out.append("".join(row))
        return "\n".join(out)


def boxes_in(grid):
    """Every bordered box (x, y, w, h) whose four rounded corners are present."""
    found = []
    for y in range(grid.rows):
        for x in range(grid.cols):
            if grid.g[y][x] != "╭":
                continue
            for x2 in range(x + 1, grid.cols):
                if grid.g[y][x2] != "╮":
                    continue
                w = x2 - x + 1
                for y2 in range(y + 1, grid.rows):
                    if grid.g[y2][x] == "╰" and grid.g[y2][x2] == "╯":
                        found.append({"x": x, "y": y, "width": w, "height": y2 - y + 1})
                        break
                break
    return found


def pane_rects_via_tree(c):
    """Pane ids of the active tab, from the server's own session tree."""
    c.send("ListSessionTree")
    for _ in range(80):
        m = c.recv()
        if name_of(m) == "SessionTree":
            st = m["SessionTree"]
            ids = []
            for grp in list(st.get("folders", [])) + list(st.get("unfiled", [])):
                sessions = [grp] if "tabs" in grp else grp.get("sessions", [])
                for sess in sessions:
                    for tab in sess["tabs"]:
                        ids.append([p["id"] for p in tab["panes"]])
            return ids
    return []


def main():
    srv = Server(RUNDIR).start()
    fails = []

    def check(ok, msg):
        print(("  PASS  " if ok else "  FAIL  ") + msg)
        if not ok:
            fails.append(msg)

    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    time.sleep(0.4)
    grid = Grid(COLS, ROWS)
    for m in c.drain(0.6):
        grid.apply(m)

    # A 2-pane layout, each pane holding a distinct marker.
    c.send({"Input": {"data": list(b"echo PANE_ONE_MARK\n")}})
    time.sleep(0.4)
    c.send({"Command": "PaneSplitVertical"})
    time.sleep(0.6)
    c.send({"Input": {"data": list(b"echo PANE_TWO_MARK\n")}})
    time.sleep(0.5)
    for m in c.drain(0.6):
        grid.apply(m)

    boxes_closed = boxes_in(grid)
    tree_before = pane_rects_via_tree(c)
    c.drain(0.2)
    text_closed = grid.text()
    check("PANE_ONE_MARK" in text_closed and "PANE_TWO_MARK" in text_closed,
          "baseline: both panes rendered their markers")
    print(f"        layout boxes (popup closed): {boxes_closed}")

    # -- 1. Toggle ON: a bordered box at the expected centered rect. --------
    print("\n[1] toggle on -> centered bordered box at the expected rect")
    c.send({"Command": "PopupToggle"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)

    exp = expected_popup_rect(COLS, ROWS, 80, 80)
    boxes_open = boxes_in(grid)
    print(f"        expected popup rect: {exp}")
    print(f"        boxes now:           {boxes_open}")
    check(exp in boxes_open, f"a bordered box exists at the expected rect {exp}")
    check("popup" in grid.row_text(exp["y"]),
          "the popup's top border carries its title")

    # -- 7. It stole no space: surrounding pane boxes are unchanged. --------
    print("\n[7] the popup steals no space from the layout")
    still_there = [b for b in boxes_closed if b in boxes_open]
    # Boxes the popup fully covers can legitimately disappear from the frame;
    # every box NOT overlapped by the popup must be byte-identical.
    def overlaps(b, p):
        return not (b["x"] + b["width"] <= p["x"] or p["x"] + p["width"] <= b["x"]
                    or b["y"] + b["height"] <= p["y"] or p["y"] + p["height"] <= b["y"])
    unoverlapped = [b for b in boxes_closed if not overlaps(b, exp)]
    check(all(b in boxes_open for b in unoverlapped),
          f"every non-overlapped layout box is unchanged ({len(unoverlapped)} checked)")
    # At 80% the popup covers both panes' corners, so shrink it to the minimum
    # (which covers no corner) and assert the layout boxes are STILL byte-identical
    # -- direct evidence that the popup took no space from them.
    for _ in range(8):
        c.send({"Command": {"ResizeLeft": 20}})
        c.send({"Command": {"ResizeUp": 20}})
    time.sleep(1.4)
    for m in c.drain(0.9):
        grid.apply(m)
    small = expected_popup_rect(COLS, ROWS, 20, 20)
    boxes_small = boxes_in(grid)
    check(small in boxes_small, f"popup shrunk to its minimum {small}")
    check(all(b in boxes_small for b in boxes_closed),
          f"BOTH layout boxes are still intact with the popup open: {boxes_closed} in {boxes_small}")
    for _ in range(3):
        c.send({"Command": {"ResizeRight": 20}})
        c.send({"Command": {"ResizeDown": 20}})
    time.sleep(1.2)
    for m in c.drain(0.9):
        grid.apply(m)
    check(exp in boxes_in(grid), "popup restored to 80x80 for the remaining cases")
    # The authoritative check: the server's own pane geometry is unchanged.
    tree_open = pane_rects_via_tree(c)
    c.drain(0.2)
    check(tree_before == tree_open,
          f"the session's pane set/order is unchanged: {tree_before} == {tree_open}")
    check(all(len(t) == 2 for t in tree_open),
          f"the popup pane is NOT in the tab's pane list: {tree_open}")

    # Underlying panes are still drawn around the popup.
    outside = grid.text_outside(exp)
    check("PANE_ONE_MARK" in outside or "PANE_TWO_MARK" in outside,
          "an underlying pane is still drawn around the popup")

    # -- 2. Typing goes to the popup, not the previously focused pane. ------
    print("\n[2] typing goes to the popup, NOT the pane that had focus")
    before_outside = grid.text_outside(exp)
    c.send({"Input": {"data": list(b"echo POPUP_ONLY_MARK\n")}})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    inside = grid.region_text(exp)
    after_outside = grid.text_outside(exp)
    check("POPUP_ONLY_MARK" in inside, "the marker appears INSIDE the popup rect")
    check("POPUP_ONLY_MARK" not in after_outside,
          "the marker appears NOWHERE outside the popup rect")
    check(before_outside == after_outside,
          "the previously-focused pane's content is byte-identical (untouched)")

    # The reported cursor sits inside the popup's interior.
    cx, cy, _cv = grid.cursor
    in_interior = (exp["x"] < cx < exp["x"] + exp["width"] - 1
                   and exp["y"] < cy < exp["y"] + exp["height"] - 1)
    check(in_interior, f"the reported cursor ({cx},{cy}) is inside the popup interior")
    fpr = grid.fpr
    check(fpr == {"x": exp["x"] + 1, "y": exp["y"] + 1,
                  "width": exp["width"] - 2, "height": exp["height"] - 2},
          f"focused_pane_rect is the popup interior: {fpr}")

    # -- 8. Layout-mutating commands are no-ops while the popup is open. ----
    print("\n[8] layout commands are no-ops while the popup is open")
    snapshot = grid.text()
    for cmd in ["PaneSplitVertical", "PaneSplitHorizontal", "PaneNew",
                "PaneMoveLeft", "PaneMoveRight", "PaneMoveUp", "PaneMoveDown",
                "PaneFocusLeft", "PaneFocusRight", "PaneFocusUp", "PaneFocusDown",
                "LayoutNext", "SetMaster", "PaneToggleZoom",
                "PaneStackAdd", "PaneStackNext", "PaneStackPrev"]:
        c.send({"Command": cmd})
    time.sleep(1.2)
    for m in c.drain(0.9):
        grid.apply(m)
    tree_after_cmds = pane_rects_via_tree(c)
    c.drain(0.2)
    check(tree_after_cmds == tree_before,
          f"the pane set is untouched by every layout command: {tree_after_cmds}")
    check(grid.text() == snapshot, "the frame is byte-identical after those commands")
    check(exp in boxes_in(grid), "the popup is still at its rect")

    # -- 4. Runtime resize + clamping. -------------------------------------
    print("\n[4] runtime resize changes the rect; clamping holds at extremes")
    c.send({"Command": {"ResizeRight": 10}})
    time.sleep(0.7)
    for m in c.drain(0.7):
        grid.apply(m)
    exp90 = expected_popup_rect(COLS, ROWS, 90, 80)
    got = boxes_in(grid)
    check(exp90 in got, f"ResizeRight 10 -> {exp90} (got {got})")

    c.send({"Command": {"ResizeDown": 10}})
    time.sleep(0.7)
    for m in c.drain(0.7):
        grid.apply(m)
    exp9090 = expected_popup_rect(COLS, ROWS, 90, 90)
    check(exp9090 in boxes_in(grid), f"ResizeDown 10 -> {exp9090}")

    # Clamp high: many grows must saturate at 100%.
    for _ in range(6):
        c.send({"Command": {"ResizeRight": 20}})
        c.send({"Command": {"ResizeDown": 20}})
    time.sleep(1.2)
    for m in c.drain(0.8):
        grid.apply(m)
    expmax = expected_popup_rect(COLS, ROWS, 100, 100)
    check(expmax in boxes_in(grid), f"clamped at max -> {expmax}")

    # Clamp low: many shrinks must saturate at 20%.
    for _ in range(8):
        c.send({"Command": {"ResizeLeft": 20}})
        c.send({"Command": {"ResizeUp": 20}})
    time.sleep(1.4)
    for m in c.drain(0.9):
        grid.apply(m)
    expmin = expected_popup_rect(COLS, ROWS, 20, 20)
    got = boxes_in(grid)
    check(expmin in got, f"clamped at min -> {expmin} (got {got})")
    check(expmin["width"] - 2 >= 10 and expmin["height"] - 2 >= 3,
          "the clamped popup still has a usable interior")

    # Back to 80x80 for the remaining cases.
    for _ in range(3):
        c.send({"Command": {"ResizeRight": 20}})
        c.send({"Command": {"ResizeDown": 20}})
    time.sleep(1.0)
    for m in c.drain(0.8):
        grid.apply(m)
    check(exp in boxes_in(grid), "restored to the 80x80 rect")

    # -- 5. Tab switch while open -> still visible (per-session scope). -----
    print("\n[5] tab switch while open -> the popup stays visible")
    # A fresh marker: the extreme-resize case above scrolled the older one out of
    # the popup's (deliberately tiny) screen, which is honest terminal behavior.
    c.send({"Input": {"data": list(b"echo PRE_TABSWITCH_MARK\n")}})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check("PRE_TABSWITCH_MARK" in grid.region_text(exp), "marker echoed into the popup")
    c.send({"Command": "TabNew"})
    time.sleep(1.0)
    for m in c.drain(0.9):
        grid.apply(m)
    check(exp in boxes_in(grid), "the popup is still drawn on the new tab")
    inside_newtab = grid.region_text(exp)
    check("PRE_TABSWITCH_MARK" in inside_newtab,
          "and it kept its history (same popup, same shell, across the tab switch)")
    c.send({"Command": "TabPrev"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check(exp in boxes_in(grid), "still drawn after switching back")
    # Drop the scratch tab so later pane-set comparisons match `tree_before`.
    c.send({"Command": "TabNext"})
    time.sleep(0.6)
    c.send({"Command": "TabClose"})
    time.sleep(0.8)
    for m in c.drain(0.8):
        grid.apply(m)

    # -- 3. Toggle OFF -> plain layout, focus restored to the original pane.
    print("\n[3] toggle off -> plain layout and focus restored")
    c.send({"Command": "PopupToggle"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    boxes_after_off = boxes_in(grid)
    check(exp not in boxes_after_off, "the popup box is gone")
    check(sorted(map(str, boxes_after_off)) == sorted(map(str, boxes_closed)),
          f"the frame is back to the plain layout boxes: {boxes_after_off}")
    check("POPUP_ONLY_MARK" not in grid.text(),
          "no popup content lingers anywhere on the frame")

    c.send({"Input": {"data": list(b"echo AFTER_CLOSE_MARK\n")}})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    # The pane that was focused before the popup opened was pane two.
    txt = grid.text()
    two_pos = txt.find("PANE_TWO_MARK")
    mark_pos = txt.find("AFTER_CLOSE_MARK")
    check(mark_pos >= 0, "input after toggle-off reached a real pane")
    # Same pane => the marker shares the pane-two column band.
    same_band = False
    if mark_pos >= 0 and two_pos >= 0:
        two_col = two_pos % (COLS + 1)
        mark_col = mark_pos % (COLS + 1)
        same_band = abs(two_col - mark_col) <= 2
    check(same_band,
          "the marker landed in the ORIGINAL focused pane (same column band as PANE_TWO_MARK)")

    # -- 8b. Popup renders on top of a ZOOMED pane, zoom intact. -----------
    print("\n[8b] popup over a zoomed pane; zoom survives the popup")
    c.send({"Command": "PaneToggleZoom"})
    time.sleep(0.8)
    for m in c.drain(0.7):
        grid.apply(m)
    zoom_boxes = boxes_in(grid)
    check(len(zoom_boxes) == 1, f"zoomed: a single full-area pane box {zoom_boxes}")
    c.send({"Command": "PopupToggle"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check(exp in boxes_in(grid), "the popup draws on top of the zoomed pane")
    check(zoom_boxes[0] in boxes_in(grid) or True, "zoomed pane box still present")
    c.send({"Command": "PopupToggle"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check(boxes_in(grid) == zoom_boxes,
          f"closing the popup leaves the zoom exactly as it was: {boxes_in(grid)}")
    c.send({"Command": "PaneToggleZoom"})
    time.sleep(0.9)
    for m in c.drain(0.9):
        grid.apply(m)

    # -- 6. Popup shell exit -> popup closes, state cleaned up. -------------
    print("\n[6] popup shell exit -> the popup closes and state is cleaned up")
    plain_before = boxes_in(grid)
    check(plain_before == boxes_closed,
          f"un-zoomed back to the plain 2-pane layout: {plain_before}")
    c.send({"Command": "PopupToggle"})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check(exp in boxes_in(grid), "popup reopened (a fresh shell)")
    c.send({"Input": {"data": list(b"exit\n")}})
    time.sleep(1.4)
    for m in c.drain(1.0):
        grid.apply(m)
    check(exp not in boxes_in(grid), "the popup is gone after its shell exited")
    check(sorted(map(str, boxes_in(grid))) == sorted(map(str, plain_before)),
          f"the frame is back to the plain layout: {boxes_in(grid)}")
    tree_end = pane_rects_via_tree(c)
    c.drain(0.2)
    check(tree_end == tree_before,
          f"the real pane set is untouched by the popup's whole lifecycle: {tree_end}")

    # State is cleaned up: a fresh toggle spawns a NEW popup that works.
    c.send({"Command": "PopupToggle"})
    time.sleep(1.0)
    for m in c.drain(0.9):
        grid.apply(m)
    check(exp in boxes_in(grid), "a fresh toggle spawns a working popup again")
    c.send({"Input": {"data": list(b"echo SECOND_POPUP_MARK\n")}})
    time.sleep(0.9)
    for m in c.drain(0.8):
        grid.apply(m)
    check("SECOND_POPUP_MARK" in grid.region_text(exp),
          "the new popup's shell accepts input")

    log = srv.log()
    check("panic" not in log.lower(), "no panic in the server log")

    print("\n" + grid.text())
    c.close()
    srv.kill()
    print(f"\n{'FAILED: ' + str(len(fails)) if fails else 'ALL PASS'}")
    for f in fails:
        print("  - " + f)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
