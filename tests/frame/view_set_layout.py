#!/usr/bin/env python3
"""Frame-level harness for choosing a View's layout by name.

Two connections, A and B, on one throwaway server with `columns = false`:

  1  `ListViewLayouts` is answered with a `LayoutList` echoing the request:
     the enabled automatic layouts in cycle order (columns absent, never
     `custom`), the View's current layout, `popup_open` false. An unknown
     View gets empty lists.
  2  A's `ViewSetLayout rows` reaches B as a `ViewList` whose View is in Rows.
  3  A disabled, a `custom` and an unknown name are refused: B receives no
     `ViewList`, the layout stays Rows, and server.log names each refusal.
  4  Setting the layout the View already has broadcasts nothing.

Run:  python3 tests/frame/view_set_layout.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_vsl"
CONFIG = """
[layouts]
columns = false
"""
EXPECTED = ["bsp", "master", "monocle", "grid", "rows"]


def view_lists(msgs):
    return [m["ViewList"]["views"] for m in msgs if name_of(m) == "ViewList"]


def layout_of(view):
    """The variant name of a serialized `LayoutMode`, lowercased."""
    layout = view["layout"]
    name = layout if isinstance(layout, str) else next(iter(layout))
    return name.lower()


def layout_lists(msgs):
    return [m["LayoutList"] for m in msgs if name_of(m) == "LayoutList"]


def main():
    srv = Server(RUNDIR).start(config=CONFIG)
    passed = []
    try:
        a = Client(srv.sock)
        b = Client(srv.sock)
        a.hello()
        b.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": 100, "rows": 30}})
        a.send({"Command": "PaneSplitVertical"})
        a.send("ListSessionTree")
        tree = next(m["SessionTree"] for m in a.drain(1.0) if name_of(m) == "SessionTree")
        panes = [p["id"] for p in tree["unfiled"][0]["tabs"][0]["panes"]]
        assert len(panes) == 2, f"expected two panes: {tree}"

        a.send({"ViewCreate": {"name": "V1"}})
        vid = next(m["ViewCreated"]["id"] for m in a.drain(0.8) if name_of(m) == "ViewCreated")
        a.send({"ViewAddCells": {"id": vid, "cells": [["Local", p] for p in panes]}})
        a.drain(0.8)
        b.drain(0.8)

        # --- 1 -----------------------------------------------------------
        a.send({"ListViewLayouts": {"request": 7, "id": vid}})
        replies = layout_lists(a.drain(0.8))
        assert len(replies) == 1, f"expected one LayoutList: {replies}"
        reply = replies[0]
        assert reply["request"] == 7, reply
        assert reply["layouts"] == EXPECTED, reply
        assert reply["current"] == "grid", reply
        assert reply["popup_open"] is False, reply
        passed.append(f"ListViewLayouts -> {reply['layouts']}, current {reply['current']}")

        a.send({"ListViewLayouts": {"request": 8, "id": vid + 100}})
        reply = layout_lists(a.drain(0.8))
        assert reply == [{"request": 8, "layouts": [], "current": "", "popup_open": False}], reply
        passed.append("an unknown View gets empty lists")

        # --- 2 -----------------------------------------------------------
        a.send({"ViewSetLayout": {"id": vid, "name": "rows"}})
        a.drain(0.8)
        lists = view_lists(b.drain(0.8))
        assert lists, "B received no ViewList after ViewSetLayout"
        view = next(v for v in lists[-1] if v["id"] == vid)
        assert layout_of(view) == "rows", view["layout"]
        assert view["custom_tree"] is None, view
        passed.append("A's ViewSetLayout rows reaches B as a ViewList in Rows")

        # --- 3 -----------------------------------------------------------
        for name in ("columns", "custom", "spiral"):
            a.send({"ViewSetLayout": {"id": vid, "name": name}})
            got = view_lists(b.drain(0.8))
            assert not got, f"{name}: B received a ViewList for a refused switch: {got}"
        a.send({"ListViewLayouts": {"request": 9, "id": vid}})
        reply = layout_lists(a.drain(0.8))
        assert reply and reply[0]["current"] == "rows", reply
        passed.append("columns / custom / spiral refused, no broadcast, still rows")

        # --- 4 -----------------------------------------------------------
        a.send({"ViewSetLayout": {"id": vid, "name": "rows"}})
        got = view_lists(b.drain(0.8))
        assert not got, f"a no-op switch was broadcast: {got}"
        passed.append("setting the current layout broadcasts nothing")

        a.close()
        b.close()
    finally:
        log = srv.log()
        srv.kill()

    for name, why in (("columns", "disabled under [layouts]"),
                      ("custom", "custom is not offered for a view"),
                      ("spiral", "unknown layout")):
        line = f'ViewSetLayout id={vid} name="{name}" refused: {why}'
        assert line in log, f"server.log lacks {line!r}"
    passed.append("server.log names every refusal")
    assert "panicked at" not in log, f"server log contains a panic:\n{log[-2000:]}"

    print("PASS view_set_layout")
    for line in passed:
        print("  ok", line)


if __name__ == "__main__":
    main()
