#!/usr/bin/env python3
"""Frame-level harness for Phase 1 of the server-side shared-view registry.

Drives the Unix socket directly (no TUI) with THREE connections A, B, C against
ONE throwaway server and asserts the four Phase-1 guarantees:

  1. Visibility broadcast: A creates a view + adds a cell; BOTH A and B see it.
  2. Live update:          A adds a second cell; B sees the updated ViewList.
  3. Mirror:               A focus/cycle-layout/zoom; B's ViewList mirrors it.
  4. Initial sync:         C connects AFTER V1 exists and receives a ViewList
                           for V1 without sending any mutation.

Run:  python3 tests/frame/shared_views_phase1.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_svp1"


def views_of(msgs):
    """Return the `views` list from the LAST ViewList in `msgs`, or None."""
    last = None
    for m in msgs:
        if name_of(m) == "ViewList":
            last = m["ViewList"]["views"]
    return last


def find_view(views, name):
    if views is None:
        return None
    for v in views:
        if v["name"] == name:
            return v
    return None


def get_pane_id(client):
    """Create a session with a pane on `client` and return the pane id."""
    client.send({"CreateSession": {"name": "main", "folder": None}})
    client.send({"Attach": {"session_name": "main"}})
    client.send({"Resize": {"cols": 100, "rows": 30}})
    client.send("ListSessionTree")
    tree = None
    for m in client.drain(0.8):
        if name_of(m) == "SessionTree":
            tree = m["SessionTree"]
    assert tree is not None, "no SessionTree reply"
    pid = None
    for sess in tree["unfiled"] + [s for f in tree["folders"] for s in f["sessions"]]:
        if sess["name"] == "main":
            pid = sess["tabs"][0]["panes"][0]["id"]
    assert pid is not None, f"no main pane in tree: {tree}"
    return pid


def main():
    srv = Server(RUNDIR).start()
    passed = []
    try:
        a = Client(srv.sock)
        b = Client(srv.sock)
        a.hello()
        b.hello()
        # Drain the initial (empty) ViewList + any handshake frames.
        a.drain(0.3)
        b.drain(0.3)

        pane_id = get_pane_id(a)
        a.drain(0.3)
        b.drain(0.3)

        # --- 1. Visibility broadcast -------------------------------------
        a.send({"ViewCreate": {"name": "V1"}})
        created_id = None
        got_viewlist_after_create = False
        for m in a.drain(0.8):
            if name_of(m) == "ViewCreated":
                created_id = m["ViewCreated"]["id"]
            if name_of(m) == "ViewList":
                got_viewlist_after_create = True
        assert created_id is not None, "A did not receive ViewCreated ack"
        assert got_viewlist_after_create, "A did not receive a ViewList after create"
        passed.append(f"ViewCreate → A got ViewCreated{{id={created_id}}} AND a ViewList")

        # Add the pane as a Local cell.
        a.send({"ViewAddCells": {"id": created_id, "cells": [["Local", pane_id]]}})
        av = find_view(views_of(a.drain(0.8)), "V1")
        bv = find_view(views_of(b.drain(0.8)), "V1")
        assert av is not None, "A's ViewList has no V1 after ViewAddCells"
        assert bv is not None, "B's ViewList has no V1 (broadcast did not reach B)"
        assert len(av["cells"]) == 1, f"A: expected 1 cell, got {av['cells']}"
        assert len(bv["cells"]) == 1, f"B: expected 1 cell, got {bv['cells']}"
        # Assert cell IDENTITY, not just count.
        assert bv["cells"][0]["pane_id"] == pane_id, f"B cell wrong pane: {bv['cells']}"
        assert bv["cells"][0]["conn"] == "Local", f"B cell wrong conn: {bv['cells']}"
        first_cell_id = bv["cells"][0]["id"]
        passed.append("Visibility broadcast: A AND B both see V1 with the Local cell (id+pane match)")

        # --- 2. Live update ----------------------------------------------
        a.send({"ViewAddCells": {"id": created_id, "cells": [["Local", pane_id]]}})
        bv = find_view(views_of(b.drain(0.8)), "V1")
        assert bv is not None and len(bv["cells"]) == 2, f"B did not see 2 cells: {bv}"
        # The two cells have DISTINCT stable ids.
        ids = [c["id"] for c in bv["cells"]]
        assert len(set(ids)) == 2, f"cell ids not distinct: {ids}"
        assert first_cell_id in ids, "the original cell id vanished from B's view"
        passed.append("Live update: A adds a 2nd cell → B sees 2 distinct cells live")

        # --- 3. Mirror focus / layout / zoom -----------------------------
        # Baseline B state.
        bv = find_view(views_of(b.drain(0.3)), "V1") or bv
        base_layout = bv["layout"]
        base_focus = bv["focused"]
        base_zoom = bv["zoomed"]

        # Focus the second cell.
        second_cell_id = [i for i in ids if i != bv["cells"][base_focus]["id"]][0]
        a.send({"ViewSetFocus": {"id": created_id, "cell_id": second_cell_id}})
        bv = find_view(views_of(b.drain(0.8)), "V1")
        assert bv["cells"][bv["focused"]]["id"] == second_cell_id, (
            f"B focus did not mirror: focused={bv['focused']} cells={bv['cells']}"
        )
        passed.append("Mirror focus: A ViewSetFocus → B's focused cell matches")

        # Cycle layout (grid → next); the layout NAME must change on B.
        a.send({"ViewCycleLayout": {"id": created_id}})
        bv = find_view(views_of(b.drain(0.8)), "V1")
        assert bv["layout"] != base_layout, (
            f"B layout did not change: {base_layout} -> {bv['layout']}"
        )
        cycled_layout = bv["layout"]
        passed.append(f"Mirror layout: A ViewCycleLayout → B sees {base_layout} → {cycled_layout}")

        # Toggle zoom.
        a.send({"ViewToggleZoom": {"id": created_id}})
        bv = find_view(views_of(b.drain(0.8)), "V1")
        assert bv["zoomed"] == (not base_zoom), f"B zoom did not mirror: {bv['zoomed']}"
        passed.append(f"Mirror zoom: A ViewToggleZoom → B sees zoomed={bv['zoomed']}")

        # --- 4. Initial sync on connect ----------------------------------
        c = Client(srv.sock)
        c.hello()  # C sends NO mutation, only the handshake.
        cv = find_view(views_of(c.drain(0.8)), "V1")
        assert cv is not None, "C did not receive a ViewList containing V1 on connect"
        assert len(cv["cells"]) == 2, f"C's V1 has wrong cell count: {cv}"
        # C sees the mirrored state (focus/layout/zoom already applied).
        assert cv["layout"] == cycled_layout, f"C layout stale: {cv['layout']}"
        assert cv["zoomed"] == (not base_zoom), f"C zoom stale: {cv['zoomed']}"
        passed.append("Initial sync: C connects (no mutation) → receives V1 with 2 cells + current state")

        a.close()
        b.close()
        c.close()
    finally:
        log = srv.log()
        srv.kill()

    # No panic in the server log.
    assert "panic" not in log.lower(), f"server log contains panic:\n{log}"

    print("PASS shared_views_phase1")
    for line in passed:
        print("  ✓", line)


if __name__ == "__main__":
    main()
