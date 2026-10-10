"""`PaneMoveToTabTarget` with a `session` moves a pane into ANOTHER session.

Exactly two sessions, so where a re-homed bystander lands is deterministic:

  main  (folder `work`): Tab 1 = C, Tab 2 = A | B with B focused (Tab 2 active)
  other:                 Tab 1 = D

Clients: `req` and `by_main` attached to `main`, `by_other` attached to
`other`, `sub` unattached with a `SubscribePane` on B.

Every tab is Monocle (`default_layout`), so a pane arriving or leaving resizes
no other pane. Under a splitting layout the resize makes each shell print, and
that output alone pushes a frame to every bystander, which would hide a move
that forgot to broadcast. A bystander's frame is read from what the server
PUSHED, never from a `Resize` that asks for one.

  1. B into `other`'s Tab 1: same pid and output, `other` Tab 1 = [D, B] with B
     focused, `main` lost B and nothing else, the requester FOLLOWS (its next
     frame names `other`), `by_main` no longer shows B, `by_other` does, and
     `sub`'s stream is labelled with `other`.
  2. A into a NEW tab of `other` (named from `other`'s count). `main`'s Tab 2 is
     emptied and removed; it was the LAST and active tab, so `main`'s active
     tab must be clamped back to Tab 1 = C, or `by_main` renders nothing.
  8. Refusals change nothing and do not re-home the requester: a closed tab id,
     a live tab id with the wrong `session`, an unknown `session`.
  9. A scrolled requester is not snapped by a refused cross move.
  7. `main`'s hidden popup is reaped when the session goes (case 3).
  3. The last pane out of `main` removes the session: gone from the tree and
     from folder `work`, absent from state.json, and the bystander `by_main`
     lands on `other`.
  4. A new tab in the SOURCE for the sole pane of a sole tab is still refused.
 10. No panic in server.log.

Markers are ASSEMBLED by the shell (`printf 'RAN:%s' b` prints `RAN:b`), so a
typed command line can never satisfy a search.

Run: python3 tests/frame/pane_move_cross_session.py
"""
import json
import os
import re
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxpmx"
COLS, ROWS = 100, 30
RENDERS = ("FullRender", "RenderDiff", "ScrollRender")
SCROLL_BY = 10

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


def last_status(msgs):
    status = None
    for m in msgs:
        n = name_of(m)
        if n in RENDERS:
            status = only(m, n).get("status") or status
    return status


def pushed(cli, g):
    """Apply what the server PUSHED since `g` was taken, without asking for a
    frame. A bystander sends nothing, so only a broadcast can reach it; a fresh
    `Resize` would make the server render the session on request and hide a
    broadcast that never happened."""
    msgs = cli.drain(0.8)
    for m in msgs:
        g.apply(m)
    return g, last_status(msgs)


def snapshot(cli):
    """A fresh full grid and the status it came with: re-`Resize` to the same
    size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    msgs = cli.drain(0.8)
    for m in msgs:
        g.apply(m)
    return g, last_status(msgs)


def cmd(cli, c):
    cli.send({"Command": c})
    return cli.drain(0.6)


def move(cli, tab_id, session):
    return cmd(cli, {"PaneMoveToTabTarget": {"tab_id": tab_id, "session": session}})


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.6)


def raw_tree(cli):
    cli.send("ListSessionTree")
    for _ in range(200):
        m = cli.recv()
        if name_of(m) == "SessionTree":
            return m["SessionTree"]
    return None


def tree(cli, session):
    """The named session's tabs as [(tab_id, name, is_active, [(pane_id, focused)])],
    or None when there is no such session."""
    st = raw_tree(cli)
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


def folder_sessions(cli, folder):
    for f in raw_tree(cli).get("folders", []):
        if f["name"] == folder:
            return [s["name"] for s in f.get("sessions", [])]
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


def saved_sessions():
    try:
        with open(f"{RUNDIR}/data/remux/state.json") as f:
            return sorted(json.load(f)["state"]["sessions"].keys())
    except (OSError, KeyError, ValueError):
        return None


def attach(cli, session):
    cli.send({"Attach": {"session_name": session}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)


def scroll_offset(msgs):
    off = None
    for m in msgs:
        n = name_of(m)
        if n in RENDERS:
            off = only(m, n).get("scroll_offset", 0)
    return off


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start(config='[appearance]\ndefault_layout = "monocle"\n')
    req, by_main, by_other, sub, sub2 = (Client(srv.sock) for _ in range(5))
    try:
        for c in (req, by_main, by_other, sub, sub2):
            c.hello()
        req.send({"CreateSession": {"name": "main", "folder": "work"}})
        attach(req, "main")
        type_line(req, "clear; printf 'RAN:%s\\n' c")
        cmd(req, "TabNew")
        type_line(req, "clear; printf 'RAN:%s\\n' a")
        cmd(req, "PaneNew")
        type_line(req, "clear; printf 'RAN:%s\\n' b; printf 'PID:%s\\n' $$")
        by_other.send({"CreateSession": {"name": "other", "folder": None}})
        attach(by_other, "other")
        type_line(by_other, "clear; printf 'RAN:%s\\n' d")
        attach(by_main, "main")

        m0 = tree(req, "main")
        o0 = tree(req, "other")
        c_pane = m0[0][3][0][0]
        a, b = m0[1][3][0][0], m0[1][3][1][0]
        d = o0[0][3][0][0]
        other_tab1 = o0[0][0]
        g, _ = snapshot(req)
        pid_before = g.find_all(r"PID:(\d+)")
        print(f"setup: main={m0} other={o0}")
        check(len(m0) == 2 and len(m0[0][3]) == 1 and len(m0[1][3]) == 2 and len(o0) == 1,
              "setup: main is Tab 1 = C, Tab 2 = A | B; other is Tab 1 = D")
        check(active_tab(m0) == m0[1][0] and focused_pane(m0) == b,
              "setup: main's Tab 2 is active with B focused")
        check(folder_sessions(req, "work") == ["main"], "setup: main is in folder work")
        check(g.find_all(r"RAN:b") and len(pid_before) == 1, "setup: B's markers are visible")

        sub.send({"SubscribePane": {"pane_id": b, "cols": 40, "rows": 12,
                                    "size_demand": False}})
        sub.drain(0.6)

        print("1. B into another session's tab")
        g_main, _ = snapshot(by_main)
        g_other, _ = snapshot(by_other)
        got = move(req, other_tab1, "other")
        st = last_status(got)
        m1 = tree(req, "main")
        o1 = tree(req, "other")
        print(f"  main={m1} other={o1} req status={st and st['session_name']}")
        check(st is not None and st["session_name"] == "other",
              "the requester follows the pane: its next frame names other")
        check([p for p, _ in o1[0][3]] == [d, b], "other's Tab 1 is now [D, B]")
        check(active_tab(o1) == other_tab1 and focused_pane(o1) == b,
              "other's Tab 1 is active with B focused")
        check(pane_ids(m1) == sorted([a, c_pane]), "main lost B and nothing else")
        check(len(m1) == 2 and active_tab(m1) == m0[1][0] and focused_pane(m1) == a,
              "main keeps Tab 2 active, now A alone")
        g, st = pushed(by_main, g_main)
        check(st is not None and st["session_name"] == "main",
              "by_main is pushed a frame of main")
        check(not g.find_all(r"RAN:b") and g.find_all(r"RAN:a"),
              "by_main's pushed frame shows A and no longer B")
        g, st = pushed(by_other, g_other)
        check(st is not None and st["session_name"] == "other" and g.find_all(r"RAN:b"),
              "by_other is pushed a frame with B arriving")
        type_line(req, "printf 'P2:%s\\n' $$; printf 'SUB:%s\\n' ok")
        g, _ = snapshot(req)
        check(g.find_all(r"P2:(\d+)") == pid_before, "the same shell runs in B")
        check(g.find_all(r"RAN:b") and not g.find_all(r"RAN:d"),
              "the requester's frame shows B, focused over D")
        contents = [only(m, "PaneContent") for m in sub.drain(1.0)
                    if name_of(m) == "PaneContent" and only(m, "PaneContent")["pane_id"] == b]
        labels = {(c["session_name"], c["tab_name"]) for c in contents}
        print(f"  subscriber labels: {labels}")
        check(contents and all(s == "other" for s, _ in labels),
              "the subscriber's stream is labelled with other")

        print("2. A into a new tab of another session; main's emptied last tab is removed")
        attach(req, "main")
        g_main, _ = snapshot(by_main)
        got = move(req, None, "other")
        st = last_status(got)
        m2 = tree(req, "main")
        o2 = tree(req, "other")
        print(f"  main={m2} other={o2}")
        check(st is not None and st["session_name"] == "other", "the requester follows A")
        check(len(o2) == 2 and o2[1][1] == "Tab 2" and [p for p, _ in o2[1][3]] == [a],
              "other has a new Tab 2 holding only A, named from other's count")
        check(len(o2) == 2 and active_tab(o2) == o2[1][0] and focused_pane(o2) == a,
              "other's Tab 2 is active")
        check(m2 is not None and len(m2) == 1 and [p for p, _ in m2[0][3]] == [c_pane]
              and m2[0][2], "main is Tab 1 = C alone, and it is the active tab")
        g, st = pushed(by_main, g_main)
        tabs = st and st["tabs"]
        print(f"  by_main status tabs: {tabs}")
        check(st is not None and st["session_name"] == "main" and len(tabs) == 1 and tabs[0][1],
              "by_main's status has exactly one tab, marked active")
        check(g.find_all(r"RAN:c"), "by_main still renders main: C's output is on the grid")

        print("8. refusals change nothing and do not re-home the requester")
        attach(req, "main")
        cmd(by_other, "TabNew")
        closed = active_tab(tree(by_other, "other"))
        cmd(by_other, "TabClose")
        before = (tree(req, "main"), tree(req, "other"))
        mark = len(srv.log())
        for tab_id, session, why in (
                (closed, "other", "TargetNotFound"),
                (other_tab1, "main", "TargetSessionMismatch"),
                (None, "nope", "NoSession")):
            got = move(req, tab_id, session)
            st = last_status(got)
            check((tree(req, "main"), tree(req, "other")) == before,
                  f"{why}: both sessions are unchanged")
            check(st is None or st["session_name"] == "main",
                  f"{why}: the requester is still on main")
            check(f"refused: {why}" in srv.log()[mark:], f"{why}: the refusal is logged")
        _, st = snapshot(req)
        check(st is not None and st["session_name"] == "main",
              "after the refusals the requester still renders main")

        print("9. a scrolled requester is not snapped by a refused cross move")
        type_line(req, "for i in $(seq 1 200); do printf 'LINE:%s\\n' $i; done")
        req.drain(1.0)
        req.send({"ScrollDelta": {"delta": SCROLL_BY}})
        off = scroll_offset(req.drain(0.8))
        check(off == SCROLL_BY, f"the requester is scrolled ({off})")
        move(req, other_tab1, "main")
        req.send({"ScrollDelta": {"delta": 1}})
        off = scroll_offset(req.drain(0.8))
        check(off == SCROLL_BY + 1, f"still scrolled after the refused move ({off})")
        req.send({"ScrollDelta": {"delta": -1000}})
        req.drain(0.6)

        print("7. main's hidden popup")
        cmd(req, "PopupToggle")
        cmd(req, "PopupToggle")
        popups = re.findall(r'PopupToggle session="main" spawned popup pane_id=(\d+)', srv.log())
        check(len(popups) == 1, f"exactly one popup was spawned for main ({popups})")
        popup = int(popups[0]) if popups else None
        if popup is not None:
            sub2.send({"SubscribePane": {"pane_id": popup, "cols": 40, "rows": 12,
                                         "size_demand": False}})
            sub2.drain(0.6)

        print("3. the last pane out of main removes the session")
        snapshot(by_main)
        got = move(req, other_tab1, "other")
        st = last_status(got)
        o3 = tree(req, "other")
        print(f"  other={o3}")
        check(st is not None and st["session_name"] == "other", "the requester follows C")
        check(tree(req, "main") is None, "main is gone from the session tree")
        check(folder_sessions(req, "work") == [], "folder work no longer lists main")
        check(c_pane in [p for p, _ in o3[0][3]], "C is in other's Tab 1")
        check(saved_sessions() == ["other"], f"state.json holds only other ({saved_sessions()})")
        _, st = pushed(by_main, Grid(COLS, ROWS))
        check(st is not None and st["session_name"] == "other",
              "the bystander by_main is pushed a frame of other")
        exited = [only(only(m, "Event"), "PaneExited") for m in sub2.drain(1.0)
                  if name_of(m) == "Event" and name_of(only(m, "Event")) == "PaneExited"]
        check(popup is not None and any(e["pane_id"] == popup for e in exited),
              f"the popup pane was reaped and its subscriber told ({exited})")

        print("4. a new tab in the source for the sole pane of a sole tab is refused")
        req.send({"CreateSession": {"name": "solo", "folder": None}})
        attach(req, "solo")
        s0 = tree(req, "solo")
        mark = len(srv.log())
        for session in ("solo", None):
            move(req, None, session)
        check(s0 is not None and len(s0) == 1 and tree(req, "solo") == s0, "nothing changed")
        check(srv.log()[mark:].count("refused: SolePaneInSoleTab") == 2,
              "both refusals are logged")

    except (OSError, ConnectionError) as e:
        check(False, f"the server stopped answering: {e!r}")
    finally:
        for c in (req, by_main, by_other, sub, sub2):
            c.close()
        srv.kill()

    print("10. health")
    check("panicked at" not in srv.log(), "no panic in server.log")

    print()
    if fails:
        print(f"FAIL: {len(fails)} check(s) failed")
        raise SystemExit(1)
    print("OK")


if __name__ == "__main__":
    main()
