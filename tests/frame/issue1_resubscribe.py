"""Issue #1 discriminator: does the server emit a fresh PaneContent for BOTH
panes when a view re-subscribes on a focus change (flipping size_demand)?

Simulates: focus was on A (A size_demand=true, B false). Focus moves to B, so
the client re-subscribes ALL cells with A false / B true. If the newly-unfocused
pane A does NOT get a fresh PaneContent, its client snapshot would be the only
thing keeping it painted -- and the task claims it goes blank.
"""
import sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxfix/i1"


def pane_ids(c):
    c.send("ListSessionTree")
    for _ in range(40):
        m = c.recv()
        if name_of(m) == "SessionTree":
            st = m["SessionTree"]
            ids = []
            for grp in list(st.get("folders", [])) + list(st.get("unfiled", [])):
                for sess in ([grp] if "tabs" in grp else grp.get("sessions", [])):
                    for tab in sess["tabs"]:
                        for p in tab["panes"]:
                            ids.append(p["id"])
            return ids
    return []


def main():
    srv = Server(RUNDIR).start()
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    time.sleep(0.3)
    c.drain(0.4)
    # Static content in pane A.
    c.send({"Input": {"data": list(b"printf 'AAAA_pane\\n'\n")}})
    time.sleep(0.3)
    c.send({"Command": "PaneSplitVertical"})
    time.sleep(0.3)
    # Focus is now on the new pane B.
    c.send({"Input": {"data": list(b"printf 'BBBB_pane\\n'\n")}})
    time.sleep(0.4)
    c.drain(0.4)

    ids = pane_ids(c)
    print("pane ids:", ids)
    if len(ids) < 2:
        print("FAIL: expected 2 panes, got", ids)
        srv.kill(); sys.exit(1)
    A, B = ids[0], ids[1]

    # Initial subscribe: A focused, B unfocused (as when the view opens on A).
    c.send({"SubscribePane": {"pane_id": A, "cols": 40, "rows": 20, "size_demand": True}})
    c.send({"SubscribePane": {"pane_id": B, "cols": 40, "rows": 20, "size_demand": False}})
    init = c.drain(0.8)
    init_pc = [only(m, "PaneContent") for m in init if name_of(m) == "PaneContent"]
    print("initial PaneContent count:", len(init_pc),
          "panes:", sorted({p["pane_id"] for p in init_pc}))

    # --- Focus change A -> B: re-subscribe ALL cells with flags flipped. ---
    c.send({"SubscribePane": {"pane_id": A, "cols": 40, "rows": 20, "size_demand": False}})
    c.send({"SubscribePane": {"pane_id": B, "cols": 40, "rows": 20, "size_demand": True}})
    after = c.drain(0.8)
    pc = [only(m, "PaneContent") for m in after if name_of(m) == "PaneContent"]
    got = {}
    for p in pc:
        got.setdefault(p["pane_id"], 0)
        got[p["pane_id"]] += 1
    print("after re-subscribe PaneContent per pane:", got)

    a_ok = got.get(A, 0) >= 1
    b_ok = got.get(B, 0) >= 1
    print(f"A({A}) fresh PaneContent on re-subscribe: {a_ok}")
    print(f"B({B}) fresh PaneContent on re-subscribe: {b_ok}")

    if "panic" in srv.log().lower():
        print("SERVER PANIC in log")
    srv.kill()
    if a_ok and b_ok:
        print("RESULT: server emits fresh PaneContent for BOTH on re-subscribe "
              "=> issue1 server path already correct")
        sys.exit(0)
    else:
        print("RESULT: server DROPS a re-subscribe frame => real server-side bug")
        sys.exit(2)


if __name__ == "__main__":
    main()
