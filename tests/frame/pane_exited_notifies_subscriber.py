#!/usr/bin/env python3
"""Frame-level harness: a subscriber is TOLD when its pane dies.

Finding 3 was a silent failure: `SubscribePane` recorded the subscription for a
pane that did not exist and sent nothing, `InputToPane` to a dead pane dropped
the keystrokes with no `else`, and nothing ever announced a pane's death. A View
cell aliasing such a pane sat on `waiting…` (or a frozen snapshot) forever.

`SessionEvent::PaneExited` (already in the wire enum, never emitted by anyone) is
now the server's answer in all four cases. This drives the socket directly with
two clients — A owns the session, B is the "view" that subscribes — and asserts
the EXACT event with the right pane id, never merely "something arrived": a
`FullRender` shows up on the close paths anyway and would make a loose assertion
pass spuriously.

  1. close by command   — A closes pane P2 → B (subscribed) gets PaneExited{P2}
  2. shell exit         — P3's shell runs `exit` → B gets PaneExited{P3}
  3. subscribe to dead  — B subscribes to the already-dead P2 → PaneExited{P2}
  4. input to dead      — B sends InputToPane{P2} → PaneExited{P2}, not silence
  5. no collateral      — the surviving pane P1 keeps streaming to B throughout

Run:  python3 tests/frame/pane_exited_notifies_subscriber.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx_pxn"


def pane_ids(c):
    """Every pane id in the tree, in tab/pane order."""
    c.send("ListSessionTree")
    tree = None
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            tree = m["SessionTree"]
    assert tree is not None, "no SessionTree reply"
    ids = []
    sessions = list(tree.get("unfiled", []))
    for f in tree.get("folders", []):
        sessions += f.get("sessions", [])
    for sess in sessions:
        for tab in sess["tabs"]:
            for p in tab["panes"]:
                ids.append(p["id"])
    return ids


def exited_ids(msgs):
    """pane_ids carried by the PaneExited events in `msgs`."""
    out = []
    for m in msgs:
        if name_of(m) == "Event":
            ev = m["Event"]
            if isinstance(ev, dict) and "PaneExited" in ev:
                out.append(ev["PaneExited"]["pane_id"])
    return out


def content_ids(msgs):
    """pane_ids carried by the PaneContent snapshots in `msgs`."""
    return [m["PaneContent"]["pane_id"] for m in msgs if name_of(m) == "PaneContent"]


def main():
    srv = Server(RUNDIR).start()
    passed = []
    try:
        a = Client(srv.sock)
        b = Client(srv.sock)
        a.hello()
        b.hello()
        a.drain(0.3)
        b.drain(0.3)

        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": 100, "rows": 30}})
        time.sleep(0.4)
        # Three panes: P1 survives everything, P2 is closed by command, P3's
        # shell exits on its own.
        a.send({"Command": "PaneSplitVertical"})
        time.sleep(0.4)
        a.send({"Command": "PaneSplitHorizontal"})
        time.sleep(0.5)
        a.drain(0.5)
        b.drain(0.3)

        ids = pane_ids(a)
        assert len(ids) == 3, f"expected 3 panes, got {ids}"
        p1, p2, p3 = ids
        a.drain(0.3)

        # B watches all three, as a three-cell view would.
        for pid in (p1, p2, p3):
            b.send({"SubscribePane": {"pane_id": pid, "cols": 40, "rows": 10,
                                      "size_demand": False}})
        got = content_ids(b.drain(0.8))
        assert set(got) >= {p1, p2, p3}, f"B did not get a snapshot for each pane: {got}"
        passed.append(f"B subscribed to all three panes {ids} and got a snapshot for each")

        # --- 1. Close by command -----------------------------------------
        a.send({"Command": {"PaneCloseById": {"session": "main", "pane_id": p2}}})
        msgs = b.drain(1.0)
        assert exited_ids(msgs) == [p2], (
            f"close of pane {p2}: expected exactly PaneExited{{{p2}}}, "
            f"got events {exited_ids(msgs)} in {[name_of(m) for m in msgs]}"
        )
        passed.append(f"PaneCloseById({p2}) → subscriber B is told: PaneExited{{pane_id={p2}}}")

        # --- 2. Shell exit ------------------------------------------------
        # P3 is the focused pane (the last split), so plain Input reaches it.
        a.send({"Input": {"data": list(b"exit\n")}})
        msgs = b.drain(1.5)
        assert exited_ids(msgs) == [p3], (
            f"shell exit in pane {p3}: expected exactly PaneExited{{{p3}}}, "
            f"got events {exited_ids(msgs)} in {[name_of(m) for m in msgs]}"
        )
        passed.append(f"shell `exit` in pane {p3} → subscriber B is told: PaneExited{{pane_id={p3}}}")

        # --- 3. Subscribe to an already-dead pane -------------------------
        b.drain(0.3)
        b.send({"SubscribePane": {"pane_id": p2, "cols": 40, "rows": 10,
                                  "size_demand": False}})
        msgs = b.drain(0.8)
        assert exited_ids(msgs) == [p2], (
            f"SubscribePane to dead {p2}: expected PaneExited{{{p2}}}, "
            f"got {[name_of(m) for m in msgs]}"
        )
        assert p2 not in content_ids(msgs), "a dead pane must not answer with content"
        passed.append(f"SubscribePane to already-dead {p2} → explicit PaneExited, not silence")

        # --- 4. Input to a dead pane --------------------------------------
        # The cleanest case: nothing else would arrive, so silence is
        # unambiguous evidence of the old bug.
        b.drain(0.3)
        b.send({"InputToPane": {"pane_id": p2, "data": list(b"hello\n")}})
        msgs = b.drain(0.8)
        assert exited_ids(msgs) == [p2], (
            f"InputToPane to dead {p2} was swallowed: got {[name_of(m) for m in msgs]}"
        )
        passed.append(f"InputToPane to dead {p2} → PaneExited answer, keystrokes not swallowed")

        # --- 5. No collateral damage --------------------------------------
        # P2 and P3 are gone, so P1 is the session's only (hence focused) pane.
        b.drain(0.3)
        a.send({"Input": {"data": list(b"printf 'STILL_ALIVE\\n'\n")}})
        msgs = b.drain(1.0)
        assert p1 in content_ids(msgs), (
            f"the surviving pane {p1} stopped streaming to B: {[name_of(m) for m in msgs]}"
        )
        assert exited_ids(msgs) == [], f"spurious PaneExited: {exited_ids(msgs)}"
        passed.append(f"surviving pane {p1} keeps streaming; no spurious PaneExited")

        a.close()
        b.close()
    finally:
        log = srv.log()
        srv.kill()

    assert "panic" not in log.lower(), f"server log contains panic:\n{log}"

    print("PASS pane_exited_notifies_subscriber")
    for line in passed:
        print("  ✓", line)


if __name__ == "__main__":
    main()
