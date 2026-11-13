"""A mouse event lands on what the clicking client SEES when a smaller client
shares its session.

Two clients on one session make the server composite the shared frame at
min(cols) x min(rows). The larger client is sent that smaller frame, so what it
shows is laid out for the smaller size, and its clicks, wheel and drags must be
hit-tested against that same layout rather than against its own size.

Each phase runs three ways: A alone, A with a second client of A's own size
(both must behave as they always did), and A with a smaller B. In each:

  1. A clicks the inactive tab's label on the row the label is DRAWN on in A's
     frame: that tab becomes active.
  2. A clicks inside the right pane as A's frame draws it: the right pane takes
     the focus. With B attached the column is left of the divider a 120-column
     layout would have, so only a hit test at the frame's size gets it right.
  3. A drags across the right pane as drawn: the yanked text is the right
     pane's (`BBB:`), not the left one's (`AAA:`).
  4. A wheels up over the right pane as drawn, with the LEFT pane focused: the
     right pane's rows in A's frame move back in its history.

The pane markers are assembled by the shell (`printf` plus a loop variable), so
the echoed command line cannot produce them, and row scans use `finditer`
because the two panes share every row.

Run: python3 tests/frame/mouse_two_client_size.py
"""
import base64
import re
import sys
from harness import Server, Client, name_of, only
from rundirs import run_dir  # noqa: E402

RUNDIR = run_dir("rmxm2c")
A_SIZE = (120, 40)
B_SIZE = (80, 24)
RENDERS = ("FullRender", "RenderDiff")
FILL = "i=0; while [ $i -lt 120 ]; do printf '{m}:%s\\n' $i; i=$((i+1)); done\n"

fails = []


def check(cond, msg):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        fails.append(msg)


class View:
    """What one client was sent: its grid, status and focused pane rect."""

    def __init__(self):
        self.grid = []
        self.status = None
        self.fpr = None
        self.yanked = []

    def apply(self, msgs):
        for m in msgs:
            n = name_of(m)
            if n == "CopyToClipboard":
                self.yanked.append(only(m, n)["data"])
            if n not in RENDERS:
                continue
            body = only(m, n)
            if n == "FullRender":
                self.grid = [[c["c"] for c in row] for row in body["cells"]]
            else:
                for ch in body["changes"]:
                    if ch["y"] < len(self.grid) and ch["x"] < len(self.grid[ch["y"]]):
                        self.grid[ch["y"]][ch["x"]] = ch["cell"]["c"]
            self.status = body.get("status") or self.status
            self.fpr = body.get("focused_pane_rect") or self.fpr
        return self

    def size(self):
        return (len(self.grid[0]) if self.grid else 0, len(self.grid))

    def row(self, y):
        return "".join(self.grid[y])

    def active_tab(self):
        tabs = (self.status or {}).get("tabs") or []
        return next((t[0] for t in tabs if t[1]), None)

    def markers(self, tag, x_from):
        """Every `tag:n` number drawn at or right of column `x_from`."""
        found = []
        for y in range(len(self.grid)):
            for m in re.finditer(rf"{tag}:(\d+)", self.row(y)):
                if m.start() >= x_from:
                    found.append(int(m.group(1)))
        return found


def click(cli, x, y):
    cli.send({"MouseClick": {"x": x, "y": y, "pane_id": None, "release": False, "popup": False}})
    cli.send({"MouseClick": {"x": x, "y": y, "pane_id": None, "release": True, "popup": False}})


def decoded(data):
    try:
        return base64.b64decode(data, validate=True).decode(errors="replace")
    except Exception:
        return data


def run(label, b_size):
    print(f"=== {label}")
    srv = Server(RUNDIR).start()
    a = Client(srv.sock)
    b = Client(srv.sock)
    try:
        a.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": A_SIZE[0], "rows": A_SIZE[1]}})
        a.drain(0.8)
        a.send({"Command": {"TabRename": "alpha"}})
        a.send({"Command": "TabNew"})
        a.drain(0.6)
        a.send({"Command": {"TabRename": "beta"}})
        a.send({"Command": "PaneSplitVertical"})
        a.drain(0.8)
        a.send({"Input": {"data": list(FILL.format(m="BBB").encode())}})
        a.drain(0.8)
        a.send({"Command": "PaneFocusLeft"})
        a.send({"Input": {"data": list(FILL.format(m="AAA").encode())}})
        a.drain(0.8)

        if b_size is not None:
            b.hello()
            b.send({"Attach": {"session_name": "main"}})
            b.send({"Resize": {"cols": b_size[0], "rows": b_size[1]}})
            b.drain(0.8)

        # A asks for a fresh frame at its own size, as a real client's
        # SIGWINCH does; the server still sends the shared min-size one.
        a.send({"Resize": {"cols": A_SIZE[0], "rows": A_SIZE[1]}})
        va = View().apply(a.drain(1.0))
        want = A_SIZE if b_size is None else (min(A_SIZE[0], b_size[0]), min(A_SIZE[1], b_size[1]))
        print(f"  A frame size {va.size()}, active tab {va.active_tab()!r}")
        check(va.size() == want, f"A is sent the shared {want} frame")
        check(va.active_tab() == "beta", "beta is the active tab before the click")
        left = va.fpr
        print(f"  focused (left) rect {left}")
        if not left:
            check(False, "a focused pane rect arrived")
            return
        # Inside the right pane as A's frame draws it, one column past the
        # divider, and on a row that holds history.
        right_x = left["x"] + left["width"] + 2
        probe_y = left["y"] + 2
        if b_size is not None and b_size[0] < A_SIZE[0]:
            check(right_x < A_SIZE[0] // 2,
                  f"probe column {right_x} is left of a {A_SIZE[0]}-col divider")

        print("--- 1. tab click on the row A sees the bar on")
        bar_y = next((y for y in range(len(va.grid) - 1, -1, -1) if "alpha" in va.row(y)), None)
        check(bar_y is not None, f"alpha is drawn in A's frame (row {bar_y})")
        if bar_y is not None:
            x = va.row(bar_y).find("alpha") + 1
            click(a, x, bar_y)
            va.apply(a.drain(0.8))
            print(f"  clicked ({x}, {bar_y}); active tab now {va.active_tab()!r}")
            check(va.active_tab() == "alpha", "a click on alpha's label makes alpha active")
            a.send({"Command": {"TabGoto": 1}})
            va.apply(a.drain(0.6))
            check(va.active_tab() == "beta", f"back on beta ({va.active_tab()!r})")

        print("--- 2. pane click in the right pane as A sees it")
        click(a, right_x, probe_y)
        va.apply(a.drain(0.8))
        print(f"  clicked ({right_x}, {probe_y}); focused rect now {va.fpr}")
        check(va.fpr is not None and va.fpr["x"] > left["x"], "the right pane takes the focus")

        print("--- 3. drag across the right pane as A sees it")
        a.send({"Command": "PaneFocusLeft"})
        va.apply(a.drain(0.6))
        va.yanked.clear()
        a.send({"MouseClick": {"x": right_x, "y": probe_y, "pane_id": None,
                               "release": False, "popup": False}})
        a.send({"MouseDrag": {"start_x": right_x, "start_y": probe_y, "end_x": right_x + 5,
                              "end_y": probe_y, "is_final": True, "pane_id": None,
                              "popup": False}})
        va.apply(a.drain(0.8))
        text = " | ".join(decoded(d) for d in va.yanked)
        print(f"  yanked {text!r}")
        check("BBB" in text and "AAA" not in text, "the drag yanks the right pane's text")

        print("--- 4. wheel over the right pane as A sees it, left focused")
        a.send({"Command": "PaneFocusLeft"})
        va.apply(a.drain(0.6))
        before = va.markers("BBB", left["x"] + left["width"])
        for _ in range(5):
            a.send({"MouseScroll": {"x": right_x, "y": probe_y, "up": True, "popup": False}})
        va.apply(a.drain(1.0))
        after = va.markers("BBB", left["x"] + left["width"])
        print(f"  right pane newest marker {max(before, default=None)} -> {max(after, default=None)}")
        check(bool(before) and bool(after) and max(after) < max(before),
              "the right pane scrolls back")

        check("panicked at" not in srv.log(), "no panic in server.log")
    finally:
        a.close()
        b.close()
        srv.kill()


def main():
    run("A alone", None)
    run("A with an equal-size B", A_SIZE)
    run(f"A with a smaller B {B_SIZE}", B_SIZE)
    if fails:
        print(f"FAILED: {len(fails)}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
