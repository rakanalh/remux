"""A `PaneRename` survives every rebuild of the layout tree.

The name a user gives a pane outranks its OSC title and its process name. It is
kept in the layout tree, and an automatic layout (Bsp, Master, Monocle, Grid,
Columns, Rows) replaces that tree with a fresh one whenever the pane set or the
mode changes. Each case renames a pane and then does one thing that rebuilds or
replaces the tree, and checks the name on BOTH surfaces: the pane's border in a
full frame, and the pane's `name` in `SessionTree`.

  1  one pane, renamed (no rebuild; the control)
  2  Bsp: rename, then `PaneNew`
  3  Bsp: rename, then `LayoutNext` (to Master)
  4  Bsp: rename, then close ANOTHER pane
  5  Master: rename, then `SetMaster` on another pane
  6  Custom: rename, then split (no rebuild; the control)
  7  Custom -> Bsp, rename there, then cycle back to the saved Custom tree
  8  Bsp: rename, then move ANOTHER pane into this tab with `PaneMoveToTabTarget`
  9  Bsp: rename, then move ANOTHER pane out of this tab to a new one
 10  Bsp: rename, then `remux stop` and start the server again (persistence)
 11  Bsp: rename, then `PaneMoveLeft`, which swaps the pane with its neighbour
 12  Bsp: rename, then zoom the renamed pane (the border is read WHILE zoomed)

Names are sent as commands, never typed, so a name on screen cannot be the
shell echoing it.

Run: python3 tests/frame/pane_rename_survives_rebuild.py
"""
import os
import shutil
import sys
import time
import subprocess
from harness import BIN, Server, Client, name_of, only

RUNDIR = f"/tmp/rmxprn-{os.getpid()}"
COLS, ROWS = 160, 40
NAME = "RN-MINE"

fails = []


def check(cond, msg):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        fails.append(msg)


def cmd(c, x, t=0.3):
    c.send({"Command": x})
    c.drain(t)


def screen_text(c):
    g = [[" "] * COLS for _ in range(ROWS)]
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in c.drain(0.8):
        n = name_of(m)
        body = only(m, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < ROWS and x < COLS:
                        g[y][x] = cell["c"]
        elif n == "RenderDiff":
            for ch in body["changes"]:
                if ch["y"] < ROWS and ch["x"] < COLS:
                    g[ch["y"]][ch["x"]] = ch["cell"]["c"]
    return "\n".join("".join(r) for r in g)


def tree_names(c, session):
    """`{pane_id: name}` for every pane of `session` in `SessionTree`."""
    c.send("ListSessionTree")
    names = {}
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            for s in only(m, "SessionTree")["unfiled"]:
                if s["name"] != session:
                    continue
                for t in s["tabs"]:
                    names.update({p["id"]: p["name"] for p in t["panes"]})
    return names


def rename(c, session):
    """Rename the focused pane to NAME and return its id."""
    cmd(c, {"PaneRename": NAME})
    ids = [pid for pid, n in tree_names(c, session).items() if n == NAME]
    check(len(ids) == 1, f"{session}: the rename landed on one pane ({ids})")
    return ids[0] if ids else None


def tab_ids(c, session):
    c.send("ListSessionTree")
    ids = []
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            for s in only(m, "SessionTree")["unfiled"]:
                if s["name"] == session:
                    ids = [t["id"] for t in s["tabs"]]
    return ids


def fresh(c, session):
    c.send({"CreateSession": {"name": session, "folder": None}})
    c.send({"Attach": {"session_name": session}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    c.drain(0.8)


def focused(c, session):
    c.send("ListSessionTree")
    out = None
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            for s in only(m, "SessionTree")["unfiled"]:
                if s["name"] != session:
                    continue
                for t in s["tabs"]:
                    if t.get("is_active"):
                        out = next((p["id"] for p in t["panes"] if p["is_focused"]), None)
    return out


def verify(c, session, label, pane_id):
    text = screen_text(c)
    names = tree_names(c, session)
    holders = [pid for pid, n in names.items() if n == NAME]
    check(NAME in text, f"{label}: the border shows {NAME}")
    check(holders == [pane_id],
          f"{label}: pane {pane_id} and only it is named {NAME} ({names})")


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    c = None
    try:
        c = Client(srv.sock)
        c.hello()

        print("--- 1 one pane, renamed")
        fresh(c, "s1")
        pid = rename(c, "s1")
        verify(c, "s1", "1", pid)

        print("--- 2 Bsp: rename, then PaneNew")
        fresh(c, "s2")
        pid = rename(c, "s2")
        cmd(c, "PaneNew")
        verify(c, "s2", "2", pid)

        print("--- 3 Bsp: rename, then LayoutNext")
        fresh(c, "s3")
        cmd(c, "PaneNew")
        pid = rename(c, "s3")
        cmd(c, "LayoutNext")
        verify(c, "s3", "3", pid)

        print("--- 4 Bsp: rename, then close another pane")
        fresh(c, "s4")
        cmd(c, "PaneNew")
        cmd(c, "PaneNew")
        pid = rename(c, "s4")
        cmd(c, "PaneFocusLeft")
        cmd(c, "PaneClose", 0.6)
        verify(c, "s4", "4", pid)

        print("--- 5 Master: rename, then SetMaster on another pane")
        fresh(c, "s5")
        cmd(c, "PaneNew")
        cmd(c, "LayoutNext")
        pid = rename(c, "s5")
        cmd(c, "PaneFocusLeft")
        cmd(c, "PaneFocusRight")
        if focused(c, "s5") == pid:
            cmd(c, "PaneFocusLeft")
        check(focused(c, "s5") not in (None, pid), "5: another pane is focused")
        cmd(c, "SetMaster")
        verify(c, "s5", "5", pid)

        print("--- 6 Custom: rename, then split")
        fresh(c, "s6")
        cmd(c, "PaneSplitVertical")
        pid = rename(c, "s6")
        cmd(c, "PaneSplitHorizontal")
        verify(c, "s6", "6", pid)

        print("--- 7 Custom -> Bsp, rename, cycle back to Custom")
        fresh(c, "s7")
        cmd(c, "PaneSplitVertical")
        cmd(c, "LayoutNext")
        pid = rename(c, "s7")
        for _ in range(6):
            cmd(c, "LayoutNext")
        status = screen_text(c).splitlines()[-1]
        check("custom" in status, f"7: back in Custom ({status.strip()[-12:]!r})")
        verify(c, "s7", "7", pid)

        print("--- 8 Bsp: rename, then move another pane into this tab")
        fresh(c, "s8")
        pid = rename(c, "s8")
        cmd(c, "TabNew", 0.6)
        first_tab = tab_ids(c, "s8")[0]
        cmd(c, {"PaneMoveToTabTarget": {"tab_id": first_tab}}, 0.6)
        check(len(tab_ids(c, "s8")) == 1, "8: the moved pane joined the first tab")
        verify(c, "s8", "8", pid)

        print("--- 9 Bsp: rename, then move ANOTHER pane out to a new tab")
        fresh(c, "s9")
        cmd(c, "PaneNew")
        cmd(c, "PaneFocusLeft")
        pid = rename(c, "s9")
        cmd(c, "PaneFocusRight")
        cmd(c, {"PaneMoveToTabTarget": {"tab_id": None}}, 0.6)
        check(len(tab_ids(c, "s9")) == 2, "9: the moved pane is in a tab of its own")
        cmd(c, "TabPrev", 0.6)
        verify(c, "s9", "9", pid)

        print("--- 11 Bsp: rename, then PaneMoveLeft swaps it with its neighbour")
        fresh(c, "s11")
        cmd(c, "PaneNew")
        pid = rename(c, "s11")
        cmd(c, "PaneMoveLeft")
        verify(c, "s11", "11", pid)

        print("--- 12 Bsp: rename, then zoom the renamed pane")
        fresh(c, "s12")
        cmd(c, "PaneNew")
        pid = rename(c, "s12")
        cmd(c, "PaneToggleZoom")
        verify(c, "s12", "12", pid)
        cmd(c, "PaneToggleZoom")

        print("--- 10 Bsp: rename, then restart the server")
        fresh(c, "s10")
        cmd(c, "PaneNew")
        pid = rename(c, "s10")
        c.close()
        subprocess.run([BIN, "stop"], env=srv.env, timeout=20,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        srv.proc.wait(timeout=20)
        srv.proc = subprocess.Popen([BIN, "server"], env=srv.env,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(200):
            if os.path.exists(srv.sock):
                break
            time.sleep(0.05)
        time.sleep(0.5)
        c = Client(srv.sock)
        c.hello()
        c.send({"Attach": {"session_name": "s10"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        c.drain(1.0)
        verify(c, "s10", "10", pid)

        check("panicked" not in srv.log(), "no panic in server.log")
    finally:
        if c is not None:
            c.close()
        srv.kill()
        shutil.rmtree(RUNDIR, ignore_errors=True)
    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
