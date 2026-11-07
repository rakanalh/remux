#!/usr/bin/env python3
"""Frame-level harness for a View's zoom, the server half.

Two connections, A and B, on a throwaway server, run once per `[zoom] mode`.
B only reads, so everything asserted on B proves a broadcast.

  1  `ViewToggleZoom` with no mode follows `[zoom] mode`; B receives the mode
     and the float percentages in a `ViewList`.
  2  A forced mode wins over `[zoom] mode`, a stored zoom keeps the mode it was
     taken with, and a toggle releases a zoom of either kind.
  3  `ViewSetFocus` on another cell releases a float and leaves a fullscreen
     zoom alone; on the focused cell it releases nothing.
  4  `ViewReleaseFloat` ends a float, and for a fullscreen zoom it does nothing
     and broadcasts nothing.
  5  Changing the layout or adding a cell releases a float and keeps a
     fullscreen zoom. Removing the floated cell releases the float, and removing
     another cell does not, on either side of it: the float stays on its pane.
  6  In Monocle a fullscreen zoom is refused, logged and not broadcast. A float
     is accepted there and a fullscreen toggle releases it.

Run:  python3 tests/frame/view_zoom.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = run_dir("rmx_vzoomf")
CONFIG = """
[zoom]
mode = "{mode}"
width = 60
height = 50
"""


def last_view(msgs, vid):
    """The View `vid` in the last `ViewList` of `msgs`, `None` with no list."""
    lists = [m["ViewList"]["views"] for m in msgs if name_of(m) == "ViewList"]
    if not lists:
        return None
    return next(v for v in lists[-1] if v["id"] == vid)


def zoom_of(view):
    """`None` for no zoom, else the mode the View reports."""
    return view.get("zoom_mode", "absent") if view["zoomed"] else None


class Rig:
    def __init__(self, a, b, vid):
        self.a, self.b, self.vid = a, b, vid

    def act(self, msg):
        """Send `msg` from A and return the View as B last heard of it."""
        self.a.send(msg)
        self.a.drain(0.3)
        view = last_view(self.b.drain(0.4), self.vid)
        assert view is not None, f"B received no ViewList after {msg}"
        return view

    def silent(self, msg):
        """Send `msg` from A and assert that B hears nothing of it."""
        self.a.send(msg)
        self.a.drain(0.3)
        view = last_view(self.b.drain(0.4), self.vid)
        assert view is None, f"{msg} was broadcast: {view}"

    def toggle(self, mode=None):
        return self.act({"ViewToggleZoom": {"id": self.vid, "mode": mode}})

    def focus(self, cell):
        return self.act({"ViewSetFocus": {"id": self.vid, "cell_id": cell}})


def run(mode):
    srv = Server(f"{RUNDIR}_{mode}").start(config=CONFIG.format(mode=mode))
    passed = []
    vid = None
    try:
        a = Client(srv.sock)
        b = Client(srv.sock)
        a.hello()
        b.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": 100, "rows": 30}})
        a.send({"Command": "PaneSplitVertical"})
        a.send({"Command": "PaneSplitVertical"})
        a.send("ListSessionTree")
        tree = next(m["SessionTree"] for m in a.drain(1.0) if name_of(m) == "SessionTree")
        panes = [p["id"] for p in tree["unfiled"][0]["tabs"][0]["panes"]]
        assert len(panes) == 3, f"expected three panes: {tree}"

        a.send({"ViewCreate": {"name": "V1"}})
        vid = next(m["ViewCreated"]["id"] for m in a.drain(0.8) if name_of(m) == "ViewCreated")
        a.send({"ViewAddCells": {"id": vid, "cells": [["Local", p] for p in panes[:2]]}})
        a.drain(0.8)
        b.drain(0.8)
        r = Rig(a, b, vid)
        release = {"ViewReleaseFloat": {"id": vid}}

        # --- 1 -----------------------------------------------------------
        view = r.toggle()
        assert zoom_of(view) == mode, view
        assert view.get("float_pct") == [60, 50], view
        assert zoom_of(r.toggle()) is None
        passed.append(f"no mode follows [zoom] ({mode}); B got it with [60, 50]")

        # --- 2 -----------------------------------------------------------
        for forced in ("float", "fullscreen"):
            assert zoom_of(r.toggle(forced)) == forced, forced
            view = r.act({"ViewRename": {"id": vid, "name": "V1"}})
            assert zoom_of(view) == forced, f"a stored {forced} zoom changed: {view}"
            other = "fullscreen" if forced == "float" else "float"
            assert zoom_of(r.toggle(other)) is None, f"{other} did not release {forced}"
        passed.append("a forced mode wins, is kept, and either kind releases either")

        # --- 3 -----------------------------------------------------------
        cells = [c["id"] for c in view["cells"]]
        assert view["focused"] == 0, view
        r.toggle("float")
        assert zoom_of(r.focus(cells[0])) == "float", "focusing the focused cell released"
        view = r.focus(cells[1])
        assert zoom_of(view) is None and view["focused"] == 1, view
        r.toggle("fullscreen")
        view = r.focus(cells[0])
        assert zoom_of(view) == "fullscreen" and view["focused"] == 0, view
        r.toggle()
        passed.append("focus on another cell ends a float, not a fullscreen zoom")

        # --- 4 -----------------------------------------------------------
        r.toggle("float")
        assert zoom_of(r.act(release)) is None, "ViewReleaseFloat left the float up"
        r.silent(release)
        r.toggle("fullscreen")
        r.silent(release)
        assert zoom_of(r.toggle()) is None, "ViewReleaseFloat ended a fullscreen zoom"
        passed.append("ViewReleaseFloat ends a float and nothing else")

        # --- 5 -----------------------------------------------------------
        r.toggle("float")
        view = r.act({"ViewSetLayout": {"id": vid, "name": "rows"}})
        assert zoom_of(view) is None, f"a layout change kept the float: {view}"
        r.toggle("float")
        assert zoom_of(r.act({"ViewCycleLayout": {"id": vid}})) is None
        r.toggle("fullscreen")
        view = r.act({"ViewSetLayout": {"id": vid, "name": "grid"}})
        assert zoom_of(view) == "fullscreen", f"a layout change ended fullscreen: {view}"
        r.toggle()

        r.toggle("float")
        view = r.act({"ViewAddCells": {"id": vid, "cells": [["Local", panes[2]]]}})
        assert zoom_of(view) is None and len(view["cells"]) == 3, view
        cells = [c["id"] for c in view["cells"]]

        r.toggle("float")
        view = r.act({"ViewRemoveCell": {"id": vid, "cell_id": cells[2]}})
        assert zoom_of(view) == "float", f"removing another cell ended the float: {view}"
        r.toggle()
        view = r.act({"ViewAddCells": {"id": vid, "cells": [["Local", panes[2]]]}})
        cells = [c["id"] for c in view["cells"]]

        r.focus(cells[1])
        view = r.toggle("float")
        lower, kept = view["cells"][0]["pane_id"], view["cells"][1]["pane_id"]
        view = r.act({"ViewRemoveCell": {"id": vid, "cell_id": cells[0]}})
        assert zoom_of(view) == "float", f"removing a lower cell ended the float: {view}"
        got = view["cells"][view["focused"]]["pane_id"]
        assert got == kept, f"the float moved from pane {kept} to pane {got}: {view}"
        r.toggle()
        view = r.act({"ViewAddCells": {"id": vid, "cells": [["Local", lower]]}})
        cells = [c["id"] for c in view["cells"]]

        r.toggle("float")
        focused = cells[view["focused"]]
        view = r.act({"ViewRemoveCell": {"id": vid, "cell_id": focused}})
        assert zoom_of(view) is None and len(view["cells"]) == 2, view
        r.toggle("fullscreen")
        view = r.act({"ViewAddCells": {"id": vid, "cells": [["Local", panes[0]]]}})
        assert zoom_of(view) == "fullscreen", f"an added cell ended fullscreen: {view}"
        r.toggle()
        passed.append("layout, add and remove end a float; fullscreen stays as it was")

        # --- 6 -----------------------------------------------------------
        r.act({"ViewSetLayout": {"id": vid, "name": "monocle"}})
        r.silent({"ViewToggleZoom": {"id": vid, "mode": "fullscreen"}})
        if mode == "fullscreen":
            r.silent({"ViewToggleZoom": {"id": vid, "mode": None}})
        assert zoom_of(r.toggle("float")) == "float", "a float was refused in Monocle"
        assert zoom_of(r.toggle("fullscreen")) is None, "a fullscreen toggle did not release"
        passed.append("Monocle refuses fullscreen, takes a float, and releases it")

        a.close()
        b.close()
    finally:
        log = srv.log()
        srv.kill()

    line = f"ViewToggleZoom id={vid} mode=Fullscreen refused:"
    assert line in log, f"server.log lacks {line!r}"
    passed.append("server.log names the refusal")
    assert "panicked at" not in log, f"server log contains a panic:\n{log[-2000:]}"

    print(f"PASS view_zoom [{mode}]")
    for line in passed:
        print("  ok", line)


if __name__ == "__main__":
    run("float")
    run("fullscreen")
