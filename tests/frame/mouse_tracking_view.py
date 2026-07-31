#!/usr/bin/env python3
"""Mouse policy in a VIEW CELL (frame harness).

A View cell is a read/write alias of a real pane, driven by the pane-scoped
messages (`ScrollPane`, `MouseClick`/`MouseDrag` with `pane_id`). Those paths
used to skip the mouse-tracking check the session wheel has always made, so:

  * the wheel over a cell running claude code / neovim did NOTHING -- it tried to
    scroll a scrollback the alternate screen does not have (assertion 1);
  * a drag over such a cell selected remux text instead of reaching the
    application (assertion 3).

Both are asserted against the RIGHT pane specifically: a second, plain-shell cell
must see no reports (assertion 2) and must keep scrolling and yanking exactly as
before (assertions 5 and 6). Coordinates in this path are already content
relative, so the expected report position is exact.

Client B never attaches -- it only subscribes, which is what a client displaying
a view does.

Run from the repo root:  python3 tests/frame/mouse_tracking_view.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_mtv"
COLS, ROWS = 100, 30
CELL_COLS, CELL_ROWS = 40, 14
REPORT = re.compile(r"\^\[\[<(\d+);(\d+);(\d+)([Mm])")


class PaneWatch:
    """Latest `PaneContent` per pane, as text."""

    def __init__(self):
        self.text = {}
        self.updates = {}

    def apply(self, msg):
        if name_of(msg) != "PaneContent":
            return
        b = msg["PaneContent"]
        pid = b["pane_id"]
        self.text[pid] = "\n".join(
            "".join(cell.get("c", " ") for cell in row) for row in b["cells"]
        )
        self.updates[pid] = self.updates.get(pid, 0) + 1

    def reports(self, pid):
        # Newlines are dropped so a report that wrapped at the cell's right edge
        # still reads as one sequence.
        flat = self.text.get(pid, "").replace("\n", "")
        return [(int(b), int(x), int(y), f) for b, x, y, f in REPORT.findall(flat)]

    def lines(self, pid, prefix="LINE"):
        return {int(m) for m in re.findall(rf"{prefix}_(\d+)", self.text.get(pid, ""))}


def pump(c, watch, t=0.5):
    msgs = c.drain(t)
    for m in msgs:
        watch.apply(m)
    return len(msgs)


def main():
    srv = Server(RUNDIR).start()
    results = []
    try:
        a = Client(srv.sock)
        a.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.3)
        a.send({"Command": "PaneSplitVertical"})
        time.sleep(0.5)
        a.drain(0.5)
        a.send("ListSessionTree")
        tree = None
        for m in a.drain(0.8):
            if name_of(m) == "SessionTree":
                tree = m["SessionTree"]
        panes = []
        for sess in tree["unfiled"] + [s for f in tree["folders"] for s in f["sessions"]]:
            if sess["name"] == "main":
                panes = [p["id"] for p in sess["tabs"][0]["panes"]]
        assert len(panes) == 2, f"expected 2 panes, got {panes}"
        tracked, plain = panes[0], panes[1]
        print("panes:", {"tracked": tracked, "plain": plain})

        # A view client: subscribed, never attached.
        b = Client(srv.sock)
        b.hello()
        watch = PaneWatch()
        for pid in panes:
            b.send({"SubscribePane": {"pane_id": pid, "cols": CELL_COLS,
                                      "rows": CELL_ROWS, "size_demand": True}})
        time.sleep(0.5)
        pump(b, watch, 0.6)

        # History in both panes; then the alt screen + SGR mouse tracking and
        # `cat -v` (which prints what it receives) in one of them.
        for pid in panes:
            b.send({"InputToPane": {"pane_id": pid,
                                    "data": list(b"for i in $(seq 1 90); do echo LINE_$i; done\n")}})
        time.sleep(1.2)
        pump(b, watch, 0.8)
        b.send({"InputToPane": {"pane_id": tracked,
                                "data": list(b"printf '\\033[?1002h\\033[?1006h\\033[?1049h'; cat -v\n")}})
        time.sleep(0.8)
        pump(b, watch, 0.6)

        # -- 1/2. Wheel over the tracking cell: forwarded, at the exact content
        # position, and only to that pane.
        b.send({"ScrollPane": {"pane_id": tracked, "up": True, "lines": 3, "x": 9, "y": 4}})
        time.sleep(0.3)
        b.send({"ScrollPane": {"pane_id": tracked, "up": False, "lines": 3, "x": 12, "y": 6}})
        time.sleep(0.3)
        pump(b, watch, 0.5)
        rs = watch.reports(tracked)
        results.append(("1. view wheel reaches a tracking app at the exact cell position",
                        rs[:2] == [(64, 10, 5, "M"), (65, 13, 7, "M")], f"{rs}"))
        results.append(("2. the other cell received nothing",
                        watch.reports(plain) == [], f"{watch.reports(plain)}"))

        # -- 3. Press / motion / release over the tracking cell.
        before = len(watch.reports(tracked))
        b.send({"MouseClick": {"pane_id": tracked, "x": 5, "y": 3, "release": False}})
        time.sleep(0.25)
        b.send({"MouseDrag": {"pane_id": tracked, "start_x": 5, "start_y": 3,
                              "end_x": 8, "end_y": 5, "is_final": False}})
        time.sleep(0.25)
        b.send({"MouseClick": {"pane_id": tracked, "x": 8, "y": 5, "release": True}})
        time.sleep(0.25)
        pump(b, watch, 0.5)
        gesture = watch.reports(tracked)[before:]
        results.append(("3. view press/motion/release reach a tracking app",
                        gesture == [(0, 6, 4, "M"), (32, 9, 6, "M"), (0, 9, 6, "m")],
                        f"{gesture}"))

        # -- 4. An edge drag on the tracking cell must not arm the repeat ticker.
        b.send({"MouseClick": {"pane_id": tracked, "x": 5, "y": 5, "release": False}})
        time.sleep(0.2)
        pump(b, watch, 0.3)
        b.send({"MouseDrag": {"pane_id": tracked, "start_x": 5, "start_y": 5,
                              "end_x": 5, "end_y": 0, "is_final": False}})
        time.sleep(0.3)
        pump(b, watch, 0.4)
        settled = len(watch.reports(tracked))
        idle = pump(b, watch, 2.0)
        results.append(("4. an edge drag on a tracking cell is not replayed by the ticker",
                        idle == 0 and len(watch.reports(tracked)) == settled,
                        f"{idle} unsolicited frames in 2s"))
        b.send({"MouseClick": {"pane_id": tracked, "x": 5, "y": 0, "release": True}})
        time.sleep(0.2)
        pump(b, watch, 0.3)

        # -- 5/6. The plain cell keeps remux's own wheel and selection.
        live = watch.lines(plain)
        b.send({"ScrollPane": {"pane_id": plain, "up": True, "lines": 6, "x": 3, "y": 3}})
        time.sleep(0.3)
        pump(b, watch, 0.5)
        scrolled = watch.lines(plain)
        results.append(("5. the wheel still scrolls a plain cell's scrollback",
                        bool(scrolled) and bool(live) and min(scrolled) < min(live),
                        f"top LINE {min(live) if live else None} -> {min(scrolled) if scrolled else None}"))

        b.send({"MouseClick": {"pane_id": plain, "x": 0, "y": 3, "release": False}})
        time.sleep(0.2)
        b.drain(0.3)
        b.send({"MouseDrag": {"pane_id": plain, "start_x": 0, "start_y": 3,
                              "end_x": 6, "end_y": 3, "is_final": True}})
        time.sleep(0.3)
        yanked = [m for m in b.drain(0.6) if name_of(m) == "CopyToClipboard"]
        results.append(("6. drag still selects and yanks in a plain cell",
                        bool(yanked), f"{yanked[:1]}"))

        results.append(("7. no panic in the server log",
                        "panic" not in srv.log().lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: view-cell mouse policy" if ok else "FAIL: view-cell mouse policy")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
