"""`PaneMoveToTabTarget` moves the focused pane, live PTY and all, to another tab.

Session `main` starts as Tab 1 = A | B/D (BSP) with B focused, and Tab 2 = C.
Moving B to Tab 2 must:

  * keep the SAME shell: the `RAN:a` marker B printed before the move is still
    on its screen after it, and `$$` inside B reports the same pid;
  * keep the pane id: the tree's pane-id set is unchanged, B now under Tab 2;
  * resize B's PTY to its new slot: `stty size` inside B afterwards equals what
    B reported when it last held a right-half slot (before D was split off),
    and differs from its quarter slot just before the move;
  * make Tab 2 active with B focused;
  * keep a `SubscribePane` subscriber (a second, unattached client) receiving
    B's content, now labelled with Tab 2's name.

Then:

  * moving the only pane of a tab removes that tab;
  * `tab_id: None` breaks the pane out into a new tab named as `TabNew` would;
  * the id of a CLOSED tab is refused and changes nothing;
  * with the popup open the move is refused and changes nothing;
  * the sole pane of a session's sole tab cannot be broken out into a new tab.

Every marker is ASSEMBLED by the shell (`printf 'RAN:%s' a` prints `RAN:a`), so
the typed command line can never satisfy the search.

Run: python3 tests/frame/pane_move_to_tab.py
"""
import json
import os
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxpmt"
COLS, ROWS = 100, 30

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    """Reconstruct the composited grid from FullRender/RenderDiff."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    self._put(y, x, cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                self._put(ch["y"], ch["x"], ch["cell"])

    def text(self):
        return ["".join(r) for r in self.g]

    def find_all(self, pattern):
        """Every match in every row. `finditer`, because two panes share a row."""
        rx = re.compile(pattern)
        return [m.groups() or m.group(0) for row in self.text() for m in rx.finditer(row)]


def snapshot(cli):
    """A fresh full grid: re-`Resize` to the same size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.5)


def move(cli, tab_id):
    cmd(cli, {"PaneMoveToTabTarget": {"tab_id": tab_id}})


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.6)


def tree(cli, session):
    """The named session's tabs as [(tab_id, name, is_active, [(pane_id, focused)])]."""
    cli.send("ListSessionTree")
    for _ in range(200):
        m = cli.recv()
        if name_of(m) != "SessionTree":
            continue
        st = m["SessionTree"]
        entries = list(st.get("unfiled", []))
        for f in st.get("folders", []):
            entries.extend(f.get("sessions", []))
        for s in entries:
            if s["name"] == session:
                return [
                    (t["id"], t["name"], t.get("is_active", False),
                     [(p["id"], p["is_focused"]) for p in t["panes"]])
                    for t in s["tabs"]
                ]
        return None
    return None


def pane_ids(t):
    return sorted(p for _, _, _, ps in t for p, _ in ps)


def focused_pane(t):
    for _, _, active, ps in t:
        if active:
            return next((p for p, f in ps if f), None)
    return None


def active_tab(t):
    return next((tid for tid, _, active, _ in t if active), None)


def saved_tabs(session):
    """The session's tabs in the persisted state.json, as {tab_id: pane_order}."""
    try:
        with open(f"{RUNDIR}/data/remux/state.json") as f:
            st = json.load(f)["state"]
        return {t["id"]: t["pane_order"] for t in st["sessions"][session]["tabs"]}
    except (OSError, KeyError, ValueError):
        return None


def content_text(msg):
    body = only(msg, "PaneContent")
    return ["".join(c.get("c", " ") for c in row) for row in body["cells"]]


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    sub = Client(srv.sock)
    try:
        cli.hello()
        sub.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)

        cmd(cli, "PaneNew")
        type_line(cli, "printf 'R:%s\\n' \"$(stty size)\"")
        g = snapshot(cli)
        right_half = g.find_all(r"R:(\d+) (\d+)")
        cmd(cli, "PaneNew")
        cmd(cli, "PaneFocusUp")
        type_line(cli, "clear; printf 'RAN:%s\\n' a; printf 'PID:%s\\n' $$; "
                       "printf 'Q:%s\\n' \"$(stty size)\"")
        t0 = tree(cli, "main")
        b = focused_pane(t0)
        tab1 = t0[0][0]

        cmd(cli, "TabNew")
        cmd(cli, {"TabGoto": 0})
        t0 = tree(cli, "main")
        tab2 = t0[1][0]
        c_pane = t0[1][3][0][0]
        g = snapshot(cli)
        pid_before = g.find_all(r"PID:(\d+)")
        quarter = g.find_all(r"Q:(\d+) (\d+)")
        print(f"setup: tree={t0} right_half={right_half} quarter={quarter} pid={pid_before}")
        check(len(t0) == 2 and len(t0[0][3]) == 3 and len(t0[1][3]) == 1,
              "setup: Tab 1 holds three panes, Tab 2 one")
        check(focused_pane(t0) == b and active_tab(t0) == tab1, "setup: B is focused in Tab 1")
        check(len(right_half) == 1 and len(quarter) == 1 and right_half != quarter,
              "setup: B reported a right-half and a smaller quarter slot")
        check(g.find_all(r"RAN:a") and len(pid_before) == 1, "setup: B's markers are visible")

        sub.send({"SubscribePane": {"pane_id": b, "cols": 40, "rows": 12,
                                    "size_demand": False}})
        sub.drain(0.6)

        print("into an existing tab")
        move(cli, tab2)
        type_line(cli, "printf 'P2:%s\\n' $$; printf 'A2:%s\\n' \"$(stty size)\"; "
                       "printf 'SUB:%s\\n' ok")
        t1 = tree(cli, "main")
        g = snapshot(cli)
        after = g.find_all(r"A2:(\d+) (\d+)")
        pid_after = g.find_all(r"P2:(\d+)")
        print(f"  tree={t1} stty={after} pid={pid_after}")
        check(pane_ids(t1) == pane_ids(t0), "no pane was created or destroyed")
        check([p for p, _ in t1[1][3]] == [c_pane, b], "B is now Tab 2's second pane")
        check(b not in [p for p, _ in t1[0][3]], "B left Tab 1")
        check(active_tab(t1) == tab2 and focused_pane(t1) == b,
              "Tab 2 is active and B is focused")
        check(g.find_all(r"RAN:a"), "output printed before the move is still on B's screen")
        check(pid_after == pid_before, f"the same shell runs in B ({pid_after} vs {pid_before})")
        check(after == right_half and after != quarter,
              f"B's PTY has its new right-half slot ({after}, was {quarter})")

        saved = saved_tabs("main")
        check(saved is not None and saved.get(tab2, [])[-1:] == [b]
              and b not in saved.get(tab1, []),
              f"the saved state records B under Tab 2 ({saved})")

        got = sub.drain(1.0)
        contents = [m for m in got if name_of(m) == "PaneContent"
                    and only(m, "PaneContent")["pane_id"] == b]
        seen_sub = any("SUB:ok" in row for m in contents for row in content_text(m))
        labels = {only(m, "PaneContent")["tab_name"] for m in contents}
        print(f"  subscriber: {len(contents)} PaneContent, tab labels {labels}")
        check(seen_sub, "the subscriber still receives B's output after the move")
        check(t1[1][1] in labels, "the subscriber's label names B's new tab")

        print("the emptied source tab is removed")
        cmd(cli, "TabNew")
        type_line(cli, "clear; printf 'RAN:%s\\n' e")
        t2 = tree(cli, "main")
        e = focused_pane(t2)
        tab3 = active_tab(t2)
        move(cli, tab1)
        t3 = tree(cli, "main")
        g = snapshot(cli)
        print(f"  tree={t3}")
        check(len(t2) == 3 and len(t3) == 2, f"three tabs became two ({len(t2)} -> {len(t3)})")
        check(tab3 not in [tid for tid, _, _, _ in t3], "the emptied tab is gone")
        check(active_tab(t3) == tab1 and focused_pane(t3) == e, "E is focused in Tab 1")
        check(pane_ids(t3) == pane_ids(t2), "no pane was created or destroyed")
        check(g.find_all(r"RAN:e"), "E's output survived the move")

        print("an emptied source tab BEFORE its target is removed")
        cli.send({"CreateSession": {"name": "order", "folder": None}})
        cli.send({"Attach": {"session_name": "order"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.6)
        type_line(cli, "printf 'RAN:%s\\n' x")
        cmd(cli, "TabNew")
        cmd(cli, {"TabGoto": 0})
        o0 = tree(cli, "order")
        x = focused_pane(o0)
        move(cli, o0[1][0])
        o1 = tree(cli, "order")
        g = snapshot(cli)
        print(f"  tree={o0} -> {o1}")
        check(o1 is not None and len(o1) == 1 and o1[0][0] == o0[1][0],
              "only the target tab is left")
        check(o1 is not None and active_tab(o1) == o0[1][0] and focused_pane(o1) == x,
              "the target is active with X focused")
        check(g.find_all(r"RAN:x"), "X's output survived the move")
        cli.send({"Attach": {"session_name": "main"}})
        cli.drain(0.6)

        print("break out into a new tab")
        move(cli, None)
        t4 = tree(cli, "main")
        g = snapshot(cli)
        print(f"  tree={t4}")
        check(len(t4) == 3, f"a third tab exists ({len(t4)})")
        new = t4[-1]
        check(new[2] and [p for p, _ in new[3]] == [e], "the new, last tab is active and holds only E")
        check(new[1] == "Tab 3", f"the new tab is named as TabNew would name it ({new[1]!r})")
        check(new[0] not in [tid for tid, _, _, _ in t3], "the new tab has a fresh id")
        check(pane_ids(t4) == pane_ids(t3), "no pane was created or destroyed")
        check(g.find_all(r"RAN:e"), "E's output survived the break-out")

        print("a closed tab's id is refused")
        cmd(cli, "TabNew")
        closed = active_tab(tree(cli, "main"))
        cmd(cli, "TabClose")
        cmd(cli, {"TabGoto": 0})
        t5 = tree(cli, "main")
        move(cli, closed)
        t6 = tree(cli, "main")
        check(closed not in [tid for tid, _, _, _ in t5], "the tab really is closed")
        check(t5 is not None and t6 == t5, "nothing changed")

        print("the move is refused while the popup is open")
        cmd(cli, "PopupToggle")
        move(cli, tab2)
        move(cli, None)
        cmd(cli, "PopupToggle")
        t7 = tree(cli, "main")
        check(t6 is not None and t7 == t6, "nothing changed")

        print("the sole pane of the sole tab cannot break out")
        cli.send({"CreateSession": {"name": "solo", "folder": None}})
        cli.send({"Attach": {"session_name": "solo"}})
        cli.drain(0.6)
        s0 = tree(cli, "solo")
        move(cli, None)
        s1 = tree(cli, "solo")
        check(s0 is not None and len(s0) == 1 and s1 == s0, "nothing changed")
    except (OSError, ConnectionError) as e:
        check(False, f"the server stopped answering: {e!r}")
    finally:
        cli.close()
        sub.close()
        srv.kill()

    log = srv.log()
    check("panicked at" not in log, "no panic in server.log")
    check("PaneMoveToTabTarget" in log and "-> tab index" in log,
          "the server log shows a move happened")
    check("refused: TargetNotFound" in log, "the closed-tab refusal is logged")
    check("refused: SolePaneInSoleTab" in log, "the sole-pane refusal is logged")
    check(re.search(r"command PaneMoveToTabTarget \{ tab_id: Some\(\d+\) \} is a no-op "
                    r"while the popup is open", log)
          and "command PaneMoveToTabTarget { tab_id: None } is a no-op while the popup is open"
          in log,
          "the popup refusal is logged")

    if fails:
        print(f"\nFAIL: {len(fails)} check(s) failed")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
