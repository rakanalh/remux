"""Popup terminal, MULTI-CLIENT (frame harness): the popup lives in session state,
so every client attached to that session sees the same popup with the same
history -- and its PTY must follow the min-across-clients render size.

The single-client harness (popup_terminal.py) can't catch a popup PTY that keeps
a stale interior size after a second, smaller client attaches: the composite area
shrinks, `popup_rect` shrinks with it, and the popup's shell would keep wrapping
at the old width.

  1. Client A attaches at 100x30 and opens the popup -> A sees it at the rect for
     100x30.
  2. Client B attaches to the SAME session at 80x24 -> BOTH clients now see the
     popup at the rect computed for 80x24 (min across clients).
  3. The popup's PTY actually followed: a line longer than the new interior wraps
     at the NEW width, not the old one.
  4. It is the SAME popup: history typed by A is visible to B, and input from B
     lands in the same shell.
  5. Toggling from B hides it for A too (one popup per session).

Run: python3 tests/frame/popup_two_client.py
"""
import sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxpop/two"
A_COLS, A_ROWS = 100, 30
B_COLS, B_ROWS = 80, 24


def expected_popup_rect(cols, rows, wpct=80, hpct=80):
    """Mirror of `popup_rect` in src/server/layout.rs."""
    aw, ah = cols, rows - 1
    w = min(max(aw * wpct // 100, 12), aw)
    h = min(max(ah * hpct // 100, 5), ah)
    return {"x": (aw - w) // 2, "y": (ah - h) // 2, "width": w, "height": h}


class Grid:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            # A FullRender may arrive at a new size; re-fit the grid.
            cells = body["cells"]
            self.rows = len(cells)
            self.cols = max((len(r) for r in cells), default=0)
            self.g = [[" "] * self.cols for _ in range(self.rows)]
            for y, row in enumerate(cells):
                for x, cell in enumerate(row):
                    self.g[y][x] = self._ch(cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])

    def text(self):
        return "\n".join("".join(r) for r in self.g)

    def region_text(self, r):
        out = []
        for y in range(r["y"] + 1, r["y"] + r["height"] - 1):
            if 0 <= y < self.rows:
                out.append("".join(self.g[y][r["x"] + 1:r["x"] + r["width"] - 1]))
        return "\n".join(out)


def boxes_in(grid):
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
                        found.append({"x": x, "y": y, "width": w,
                                      "height": y2 - y + 1})
                        break
                break
    return found


def main():
    srv = Server(RUNDIR).start()
    fails = []

    def check(ok, msg):
        print(("  PASS  " if ok else "  FAIL  ") + msg)
        if not ok:
            fails.append(msg)

    ga, gb = Grid(A_COLS, A_ROWS), Grid(B_COLS, B_ROWS)

    # -- 1. Client A opens the popup at 100x30. ----------------------------
    print("[1] client A (100x30) opens the popup")
    a = Client(srv.sock)
    a.hello()
    a.send({"CreateSession": {"name": "main", "folder": None}})
    a.send({"Attach": {"session_name": "main"}})
    a.send({"Resize": {"cols": A_COLS, "rows": A_ROWS}})
    time.sleep(0.5)
    for m in a.drain(0.6):
        ga.apply(m)

    a.send({"Command": "PopupToggle"})
    time.sleep(1.0)
    for m in a.drain(0.8):
        ga.apply(m)
    exp_a = expected_popup_rect(A_COLS, A_ROWS)
    check(exp_a in boxes_in(ga), f"A sees the popup at {exp_a} (boxes {boxes_in(ga)})")

    a.send({"Input": {"data": list(b"echo FROM_CLIENT_A\n")}})
    time.sleep(0.9)
    for m in a.drain(0.8):
        ga.apply(m)
    check("FROM_CLIENT_A" in ga.region_text(exp_a), "A's input went into the popup")

    # -- 2. Client B attaches to the SAME session at 80x24. ----------------
    print("\n[2] client B (80x24) attaches to the same session")
    b = Client(srv.sock)
    b.hello()
    b.send({"Resize": {"cols": B_COLS, "rows": B_ROWS}})
    b.send({"Attach": {"session_name": "main"}})
    time.sleep(1.0)
    for m in b.drain(0.9):
        gb.apply(m)
    for m in a.drain(0.9):
        ga.apply(m)

    exp_min = expected_popup_rect(B_COLS, B_ROWS)
    print(f"        rect for min dims (80x24): {exp_min}")
    print(f"        B boxes: {boxes_in(gb)}")
    print(f"        A boxes: {boxes_in(ga)}")
    check(exp_min in boxes_in(gb), f"B sees the popup at the min-dims rect {exp_min}")
    check(exp_min in boxes_in(ga),
          "A ALSO re-renders the popup at the min-dims rect (shared session state)")

    # -- 4. Same popup, same shell: B sees A's history. --------------------
    print("\n[4] it is the SAME popup (shared session state)")
    check("FROM_CLIENT_A" in gb.region_text(exp_min),
          "B sees the history A typed into the popup")

    # -- 3. The popup's PTY followed the new size. ------------------------
    print("\n[3] the popup's PTY tracks the new (smaller) interior width")
    interior_w = exp_min["width"] - 2
    # A line comfortably longer than the new interior but shorter than the old
    # one: it can only wrap if the PTY/screen really is the new width.
    n = interior_w + 12
    a.send({"Input": {"data": list(f"printf 'W%.0s' $(seq 1 {n}); echo\n".encode())}})
    time.sleep(1.2)
    for m in b.drain(1.0):
        gb.apply(m)
    for m in a.drain(0.6):
        ga.apply(m)
    inside_b = gb.region_text(exp_min)
    wrapped_rows = [line for line in inside_b.split("\n") if line.count("W") > 0]
    longest_run = 0
    for line in inside_b.split("\n"):
        cur = 0
        for ch in line:
            cur = cur + 1 if ch == "W" else 0
            longest_run = max(longest_run, cur)
    print(f"        new interior width={interior_w}, printed {n} W's, "
          f"longest unbroken run on one row={longest_run}, rows with W={len(wrapped_rows)}")
    check(longest_run <= interior_w,
          f"no row holds more than the new interior width ({longest_run} <= {interior_w})")
    check(len(wrapped_rows) >= 2,
          f"the line wrapped across rows ({len(wrapped_rows)} rows) -> the PTY really resized")

    # Input from B reaches the same shell.
    b.send({"Input": {"data": list(b"echo FROM_CLIENT_B\n")}})
    time.sleep(1.0)
    for m in b.drain(0.8):
        gb.apply(m)
    for m in a.drain(0.8):
        ga.apply(m)
    check("FROM_CLIENT_B" in gb.region_text(exp_min), "B's input reached the popup")
    check("FROM_CLIENT_B" in ga.region_text(exp_min),
          "and A sees it too -> one shared popup shell")

    # -- 5. Toggling from B hides it for A as well. ------------------------
    print("\n[5] toggling from B hides the popup for BOTH clients")
    b.send({"Command": "PopupToggle"})
    time.sleep(1.0)
    for m in b.drain(0.9):
        gb.apply(m)
    for m in a.drain(0.9):
        ga.apply(m)
    check(exp_min not in boxes_in(gb), "B no longer shows the popup")
    check(exp_min not in boxes_in(ga), "A no longer shows the popup either")
    check("FROM_CLIENT_B" not in ga.text() and "FROM_CLIENT_B" not in gb.text(),
          "no popup content lingers on either frame")

    log = srv.log()
    check("panic" not in log.lower(), "no panic in the server log")

    print("\n--- A ---\n" + ga.text())
    print("\n--- B ---\n" + gb.text())
    a.close()
    b.close()
    srv.kill()
    print(f"\n{'FAILED: ' + str(len(fails)) if fails else 'ALL PASS'}")
    for f in fails:
        print("  - " + f)
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
