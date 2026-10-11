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

The source tab is the LAST tab of `main`. The daemon always moves the active
tab's focused pane, so the source tab is always the active one, and only when
it is also the last does removing it leave `main`'s active tab out of range.
That is the state case 2 must see repaired.

Cases, in the order they run:

  1. B into `other`'s Tab 1: same pid and output, `other` Tab 1 = [D, B] with B
     focused, `main` lost B and nothing else, the requester FOLLOWS (its next
     frame names `other`), `by_main` is pushed a frame without B, `by_other`
     one with B, and `sub`'s stream is labelled with `other`.
  5. Output typed into B now is pushed to `by_other`, and `main` is not
     re-rendered for it: the forwarder follows the pane's session.
 5b. B sets an OSC 2 title. The output itself is a batch, which case 5 covers;
     the redraw that shows the title comes when it SETTLES, from the
     forwarder's quiet path, and it too must go to `other` and not to `main`.
  6. B's shell exits: B leaves `other`'s Tab 1 within 2s, and `sub` is told.
  2. A into a NEW tab of `other` (named from `other`'s count). `main`'s Tab 2 is
     emptied and removed; it was the last and active tab, so `main`'s active
     tab must be clamped back to Tab 1 = C, or `by_main` renders nothing.
  8. Refusals change nothing and do not re-home the requester: a closed tab id,
     a live tab id with the wrong `session`, an unknown `session`.
  9. A scrolled requester is not snapped by a refused cross move.
  7. `main`'s popup is spawned and hidden, so it is in no tab and not in
     state.json; its id comes from the server log.
  3. The last pane out of `main` removes the session: gone from the tree, from
     folder `work` and from state.json, its popup reaped and its subscriber
     told, and the bystander `by_main`, scrolled back on C when C moves, is
     pushed a frame of `other` at the live tail. A scroll on the focused pane
     makes the broadcast skip a client, so a scroll left behind would freeze it.
  4. A new tab in the SOURCE for the sole pane of a sole tab is still refused.
 6b. A, the only pane of its tab in `other`, exits: the tab closes with it.
 10. No panic in server.log.
 12. A bystander scrolled back on a pane that moves away and later comes back
     to it is not frozen by the scroll it left behind, on its own server.
 11. Cases 5 and 6 on a RESTORED server: sessions saved, the server killed and
     started again over the same state, so every pane's PTY is spawned by the
     restore path rather than by a command.

Not covered: an old (protocol 16) server receiving `session`. It needs a
server built before protocol 17, and the current client refuses such a server
at the handshake, which is the protection the bump exists to give.

Markers are ASSEMBLED by the shell (`printf 'RAN:%s' b` prints `RAN:b`), so a
typed command line can never satisfy a search.

Run: python3 tests/frame/pane_move_cross_session.py
"""
import json
import os
import re
import subprocess
import time
from harness import BIN, Server, Client, name_of, only

RUNDIR = "/tmp/rmxpmx"
RESTORE_RUNDIR = "/tmp/rmxpmxr"
SCROLL_RUNDIR = "/tmp/rmxpmxs"
MONOCLE = '[appearance]\ndefault_layout = "monocle"\n'
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
    return saved_sessions_in(RUNDIR)


def saved_sessions_in(rundir):
    try:
        with open(f"{rundir}/data/remux/state.json") as f:
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


def restart(srv):
    """Kill the server and start a new one over the same directories, so it
    restores the saved sessions. `Server.start` would wipe them."""
    srv.kill()
    if os.path.exists(srv.sock):
        os.unlink(srv.sock)
    srv.proc = subprocess.Popen([BIN, "server"], env=srv.env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(srv.sock):
            time.sleep(0.5)
            return
        time.sleep(0.05)
    raise SystemExit("the restarted server made no socket")


def scroll_left_behind():
    print("12. a bystander's scroll on a pane that moves away is not left behind")
    srv = Server(SCROLL_RUNDIR).start(config=MONOCLE)
    clients = []
    try:
        req, by_main = Client(srv.sock), Client(srv.sock)
        clients = [req, by_main]
        req.hello()
        by_main.hello()
        req.send({"CreateSession": {"name": "main", "folder": None}})
        attach(req, "main")
        cmd(req, "PaneNew")
        type_line(req, "for i in $(seq 1 200); do printf 'LINE:%s\\n' $i; done")
        req.drain(1.0)
        req.send({"CreateSession": {"name": "other", "folder": None}})
        req.drain(0.6)
        attach(by_main, "main")
        m0, o0 = tree(req, "main"), tree(req, "other")
        b = focused_pane(m0)
        by_main.send({"ScrollDelta": {"delta": SCROLL_BY}})
        off = scroll_offset(by_main.drain(0.8))
        check(off == SCROLL_BY, f"scroll: by_main is scrolled back on B ({off})")

        move(req, o0[0][0], "other")
        m1 = tree(req, "main")
        check(b not in pane_ids(m1) and len(pane_ids(m1)) == 1,
              f"scroll: B left main, which survives ({m1})")
        by_main.drain(0.6)
        move(req, m1[0][0], "main")
        m2 = tree(req, "main")
        check(focused_pane(m2) == b, f"scroll: B is back in main and focused ({m2})")
        msgs = by_main.drain(1.0)
        st = last_status(msgs)
        check(st is not None and st["session_name"] == "main",
              "scroll: by_main is pushed a frame when B comes back")
        check(scroll_offset(msgs) == 0,
              f"scroll: at the live tail, not the offset left behind ({scroll_offset(msgs)})")
    except (OSError, ConnectionError) as e:
        check(False, f"scroll: the server stopped answering: {e!r}")
    finally:
        for c in clients:
            c.close()
        srv.kill()
    check("panicked at" not in srv.log(), "scroll: no panic in server.log")


def restored():
    print("11. the same move on a restored server")
    srv = Server(RESTORE_RUNDIR).start(config=MONOCLE)
    clients = []
    try:
        seed = Client(srv.sock)
        seed.hello()
        seed.send({"CreateSession": {"name": "main", "folder": None}})
        attach(seed, "main")
        cmd(seed, "PaneNew")
        seed.send({"CreateSession": {"name": "other", "folder": None}})
        seed.drain(0.8)
        seed.close()
        check(saved_sessions_in(RESTORE_RUNDIR) == ["main", "other"],
              "restore: both sessions are saved before the restart")

        restart(srv)
        req, by_other = Client(srv.sock), Client(srv.sock)
        clients = [req, by_other]
        req.hello()
        by_other.hello()
        attach(req, "main")
        attach(by_other, "other")
        m0, o0 = tree(req, "main"), tree(req, "other")
        print(f"  restored: main={m0} other={o0}")
        b = focused_pane(m0)
        d = o0[0][3][0][0] if o0 else None
        check(m0 is not None and len(m0[0][3]) == 2 and o0 is not None,
              "restore: main has two panes and other exists")
        check("restored 2 sessions" in srv.log(), "restore: the server restored them")

        move(req, o0[0][0], "other")
        g_other, _ = snapshot(by_other)
        type_line(req, "printf 'POST:%s\\n' r")
        g, st = pushed(by_other, g_other)
        check(st is not None and st["session_name"] == "other" and g.find_all(r"POST:r"),
              "restore: by_other is pushed the moved pane's new output")

        type_line(req, "exit")
        o1 = None
        for _ in range(10):
            o1 = tree(req, "other")
            if b not in pane_ids(o1):
                break
            req.drain(0.2)
        print(f"  other={o1}")
        check(b not in pane_ids(o1) and [p for p, _ in o1[0][3]] == [d],
              "restore: the moved pane's exit closes it in other within 2s")
    except (OSError, ConnectionError) as e:
        check(False, f"restore: the server stopped answering: {e!r}")
    finally:
        for c in clients:
            c.close()
        srv.kill()
    check("panicked at" not in srv.log(), "restore: no panic in server.log")


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start(config=MONOCLE)
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

        print("5. output from the moved pane is composited onto the TARGET's frame")
        g_main, _ = snapshot(by_main)
        g_other, _ = snapshot(by_other)
        type_line(req, "printf 'POST:%s\\n' x")
        g, st = pushed(by_other, g_other)
        check(st is not None and st["session_name"] == "other" and g.find_all(r"POST:x"),
              "by_other is pushed B's new output")
        _, st = pushed(by_main, g_main)
        check(st is None, f"main is not re-rendered for B's output ({st and st['session_name']})")

        print("5b. a title the moved pane sets is redrawn on the TARGET when it settles")
        g_main, _ = snapshot(by_main)
        g_other, _ = snapshot(by_other)
        type_line(req, "printf '\\033]2;TTL%s\\007' x")
        time.sleep(2.0)
        g, st = pushed(by_other, g_other)
        check(st is not None and g.find_all(r"TTLx"),
              "by_other is pushed a frame naming B by its settled title")
        _, st = pushed(by_main, g_main)
        check(st is None, f"main is not re-rendered for B's title ({st and st['session_name']})")

        print("6. the moved pane's exit closes it in the TARGET")
        type_line(req, "exit")
        o6 = None
        for _ in range(10):
            o6 = tree(req, "other")
            if b not in pane_ids(o6):
                break
            req.drain(0.2)
        print(f"  other={o6}")
        check(b not in pane_ids(o6) and [p for p, _ in o6[0][3]] == [d],
              "B left other's Tab 1 within 2s, leaving D")
        exited = [only(only(m, "Event"), "PaneExited") for m in sub.drain(1.0)
                  if name_of(m) == "Event" and name_of(only(m, "Event")) == "PaneExited"]
        check(any(e["pane_id"] == b for e in exited),
              f"B's subscriber is told it exited ({exited})")

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
        by_main.send({"ScrollDelta": {"delta": SCROLL_BY}})
        off = scroll_offset(by_main.drain(0.8))
        check(off == SCROLL_BY, f"by_main is scrolled back on C ({off})")
        got = move(req, other_tab1, "other")
        st = last_status(got)
        o3 = tree(req, "other")
        print(f"  other={o3}")
        check(st is not None and st["session_name"] == "other", "the requester follows C")
        check(tree(req, "main") is None, "main is gone from the session tree")
        check(folder_sessions(req, "work") == [], "folder work no longer lists main")
        check(c_pane in [p for p, _ in o3[0][3]], "C is in other's Tab 1")
        check(saved_sessions() == ["other"], f"state.json holds only other ({saved_sessions()})")
        msgs = by_main.drain(0.8)
        st = last_status(msgs)
        check(st is not None and st["session_name"] == "other",
              "the bystander by_main is pushed a frame of other")
        check(scroll_offset(msgs) == 0,
              f"by_main's frame of other is at the live tail ({scroll_offset(msgs)})")
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

        print("6b. a moved pane that is its tab's sole pane closes the tab on exit")
        attach(req, "other")
        o_before = tree(req, "other")
        a_tab = next(tid for tid, _, _, ps in o_before if [p for p, _ in ps] == [a])
        cmd(req, {"TabGoto": [tid for tid, _, _, _ in o_before].index(a_tab)})
        type_line(req, "exit")
        o_after = None
        for _ in range(10):
            o_after = tree(req, "other")
            if a_tab not in [tid for tid, _, _, _ in o_after]:
                break
            req.drain(0.2)
        print(f"  other={o_before} -> {o_after}")
        check(a_tab not in [tid for tid, _, _, _ in o_after] and a not in pane_ids(o_after),
              "A's tab closed with it, within 2s")

    except (OSError, ConnectionError) as e:
        check(False, f"the server stopped answering: {e!r}")
    finally:
        for c in (req, by_main, by_other, sub, sub2):
            c.close()
        srv.kill()

    print("10. health")
    check("panicked at" not in srv.log(), "no panic in server.log")

    scroll_left_behind()
    restored()

    print()
    if fails:
        print(f"FAIL: {len(fails)} check(s) failed")
        raise SystemExit(1)
    print("OK")


if __name__ == "__main__":
    main()
