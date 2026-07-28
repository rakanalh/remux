"""Part B (frame-level): a View cell must NOT shrink a pane that is being viewed
full-size in its real session ("session-visible"), and must flip live between the
"Active in session" placeholder state and cell-sized live content as visibility
changes.

Two socket clients:
  - client1 ATTACHES session `main` (pane P in its active tab) -> P is
    session-visible.
  - client2 is a pure viewer: it SubscribePane's P with size_demand=true (as a
    focused View cell does).

Assertions, read straight off the `PaneContent` messages client2 receives:
  1. While P is session-visible: PaneContent carries session_visible=true AND is
     the HOME size (~98 cols), NOT the 40x20 the cell asked for -> the cell did
     not shrink the pane.
  2. client1 switches to another tab -> P leaves the active tab -> client2 gets a
     fresh PaneContent with session_visible=false AND cols/rows == 40x20 -> the
     pane reflowed to the cell.
  3. client1 switches back -> session_visible=true again AND full size restored.
"""
import sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxais/partb"


def first_pane_id(c):
    c.send("ListSessionTree")
    for _ in range(60):
        m = c.recv()
        if name_of(m) == "SessionTree":
            st = m["SessionTree"]
            for grp in list(st.get("folders", [])) + list(st.get("unfiled", [])):
                sessions = [grp] if "tabs" in grp else grp.get("sessions", [])
                for sess in sessions:
                    for tab in sess["tabs"]:
                        for p in tab["panes"]:
                            return p["id"]
    return None


def last_pane_content(msgs, pane_id):
    """The last PaneContent for pane_id in msgs, or None."""
    found = None
    for m in msgs:
        if name_of(m) == "PaneContent":
            pc = only(m, "PaneContent")
            if pc["pane_id"] == pane_id:
                found = pc
    return found


def main():
    srv = Server(RUNDIR).start()
    fails = []

    # -- client1: owns/views the session, making P session-visible. --
    c1 = Client(srv.sock)
    c1.hello()
    c1.send({"CreateSession": {"name": "main", "folder": None}})
    c1.send({"Attach": {"session_name": "main"}})
    c1.send({"Resize": {"cols": 100, "rows": 30}})
    time.sleep(0.3)
    # Add a second tab to switch to later, then return so P's Tab 1 is active.
    c1.send({"Command": "TabNew"})
    time.sleep(0.3)
    c1.send({"Command": "TabPrev"})
    time.sleep(0.3)
    c1.drain(0.4)

    P = first_pane_id(c1)
    print("pane P id:", P)
    if P is None:
        print("FAIL: could not resolve pane id")
        srv.kill(); sys.exit(1)

    # -- client2: a pure viewer subscribing P as a focused cell (size_demand). --
    c2 = Client(srv.sock)
    c2.hello()
    c2.send({"SubscribePane": {"pane_id": P, "cols": 40, "rows": 20, "size_demand": True}})
    got = c2.drain(0.8)
    pc = last_pane_content(got, P)
    print("[1 visible] PaneContent:", None if pc is None else
          {"session_visible": pc["session_visible"], "cols": pc["cols"], "rows": pc["rows"]})
    if pc is None:
        fails.append("no PaneContent while visible")
    else:
        if not pc["session_visible"]:
            fails.append("session_visible should be TRUE while P is in an attached client's active tab")
        if pc["cols"] <= 40:
            fails.append(f"pane was shrunk to {pc['cols']} cols while session-visible (should stay ~home size)")

    home_cols = pc["cols"] if pc else 0

    # -- Flip: client1 switches away from P's tab -> P no longer session-visible. --
    c2.drain(0.2)  # clear
    c1.send({"Command": "TabNext"})
    time.sleep(0.3)
    got = c2.drain(0.8)
    pc = last_pane_content(got, P)
    print("[2 hidden] PaneContent:", None if pc is None else
          {"session_visible": pc["session_visible"], "cols": pc["cols"], "rows": pc["rows"]})
    if pc is None:
        fails.append("no fresh PaneContent when P left the active tab")
    else:
        if pc["session_visible"]:
            fails.append("session_visible should be FALSE after P left the active tab")
        if not (pc["cols"] == 40 and pc["rows"] == 20):
            fails.append(f"pane did not reflow to the cell (got {pc['cols']}x{pc['rows']}, want 40x20)")

    # -- Flip back: client1 returns to P's tab -> session-visible again. --
    c2.drain(0.2)
    c1.send({"Command": "TabPrev"})
    time.sleep(0.3)
    got = c2.drain(0.8)
    pc = last_pane_content(got, P)
    print("[3 visible again] PaneContent:", None if pc is None else
          {"session_visible": pc["session_visible"], "cols": pc["cols"], "rows": pc["rows"]})
    if pc is None:
        fails.append("no fresh PaneContent when P returned to the active tab")
    else:
        if not pc["session_visible"]:
            fails.append("session_visible should be TRUE again after returning to P's tab")
        if pc["cols"] <= 40:
            fails.append(f"pane not restored to full size (got {pc['cols']}, home was {home_cols})")

    if "panic" in srv.log().lower():
        fails.append("server panic in log")
    srv.kill()

    if fails:
        print("FAIL:")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("PASS: session-visible cell keeps pane full size + placeholder; "
          "flips to cell-sized live content off-tab and back")


if __name__ == "__main__":
    main()
