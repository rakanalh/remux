"""`TabMoveToSession` moves a whole tab, with its panes, to another session.

  main: Tab 1 = C, Tab 2 = A | B (active)
  other: Tab 1 = D

Clients: `req` and `by_main` attached to `main`, `by_other` attached to `other`,
`sub` unattached with a `SubscribePane` on B, as a View cell would hold it.

Every tab is Monocle, so a tab arriving or leaving resizes no other pane. Under
a splitting layout the resize makes each shell print, and that output alone
pushes a frame to every bystander, which would hide a move that forgot to
broadcast. A bystander's frame is read from what the server PUSHED.

Cases, in the order they run:

  1. Refusals change nothing: unknown tab, unknown target, a stale `from`, the
     source as the target. A scrolled requester is not snapped by one.
  2. The active tab into `other`: same tab id, panes and pids, the requester
     FOLLOWS, `main` falls back to Tab 1, `by_main` is pushed a frame of it and
     `by_other` one showing the arrived tab.
  3. The moved panes work: a marker typed into B is pushed to `other`'s clients
     and `sub`'s stream, input through `InputToPane` reaches it, and `main` is
     not re-rendered for it.
  4. A background tab: the requester stays where it is and the target keeps its
     active tab.
  5. The only tab of a session: the session goes from the tree and from
     state.json, its bystander is re-homed, and the requester lands in the
     target rather than wherever `handle_session_removed` would send it.
  6. A bystander scrolled back on a moved pane is pushed live frames afterwards.
  7. An unattached requester can move a tab, as a remote's panel does.
  8. No panic in server.log.

Markers are ASSEMBLED by the shell (`printf 'RAN:%s' b` prints `RAN:b`).

Run from the repo root: PYTHONPATH=tests/frame python3 tests/frame/tab_move_session.py
"""
import json
import os
import sys
import time

from harness import Server, Client, name_of, only

RUNDIR = os.environ.get("REMUX_TMS_RUN", "/tmp/rmxtms")
MONOCLE = '[appearance]\ndefault_layout = "monocle"\n'
COLS, ROWS = 100, 30
RENDERS = ("FullRender", "RenderDiff", "ScrollRender")

fails = []


def check(cond, msg):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        fails.append(msg)


def status_of(msgs):
    status = None
    for m in msgs:
        n = name_of(m)
        if n in RENDERS:
            status = only(m, n).get("status") or status
    return status


def text_of(msgs):
    """Every character the frames carried, row by row where a row is whole."""
    out = []
    for m in msgs:
        n = name_of(m)
        body = only(m, n) if n in RENDERS else None
        if n == "FullRender":
            out += ["".join(c.get("c", " ") for c in row) for row in body["cells"]]
        elif n == "RenderDiff":
            out.append("".join(ch["cell"].get("c", " ") for ch in body["changes"]))
    return "\n".join(out)


def rendered(msgs):
    return [m for m in msgs if name_of(m) in RENDERS]


def cmd(cli, c):
    cli.send({"Command": c})
    return cli.drain(0.6)


def move(cli, tab_id, source, target):
    return cmd(cli, {"TabMoveToSession": {"tab_id": tab_id, "from": source, "to": target}})


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.6)


def attach(cli, session):
    cli.send({"Attach": {"session_name": session}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)


def raw_tree(cli):
    cli.send("ListSessionTree")
    for _ in range(400):
        m = cli.recv()
        if name_of(m) == "SessionTree":
            return m["SessionTree"]
    raise SystemExit("no SessionTree reply")


def tree(cli):
    """{session: [(tab id, is_active, [pane ids])]}"""
    t = raw_tree(cli)
    entries = list(t.get("unfiled", []))
    for f in t.get("folders", []):
        entries.extend(f.get("sessions", []))
    return {
        s["name"]: [(tab["id"], tab.get("is_active"), [p["id"] for p in tab["panes"]])
                    for tab in s["tabs"]]
        for s in entries
    }


def active(tabs):
    return next(t for t in tabs if t[1])


def current(cli):
    t = raw_tree(cli)
    entries = list(t.get("unfiled", []))
    for f in t.get("folders", []):
        entries.extend(f.get("sessions", []))
    return next((s["name"] for s in entries if s.get("is_current")), None)


def saved_sessions():
    with open(f"{RUNDIR}/data/remux/state.json") as f:
        return sorted(json.load(f)["state"]["sessions"].keys())


def saved_tabs(session):
    with open(f"{RUNDIR}/data/remux/state.json") as f:
        return [t["id"] for t in json.load(f)["state"]["sessions"][session]["tabs"]]


def pane_contents(msgs, pane):
    out = []
    for m in msgs:
        if name_of(m) == "PaneContent" and only(m, "PaneContent")["pane_id"] == pane:
            cells = only(m, "PaneContent")["cells"]
            out += ["".join(c.get("c", " ") for c in row) for row in cells]
    return "\n".join(out)


def main():
    srv = Server(RUNDIR).start(config=MONOCLE)
    req, by_main, by_other, sub, loose = (Client(srv.sock) for _ in range(5))
    try:
        for c in (req, by_main, by_other, sub, loose):
            c.hello()
        req.send({"CreateSession": {"name": "main", "folder": None}})
        attach(req, "main")
        cmd(req, "TabNew")
        cmd(req, "PaneNew")
        by_other.send({"CreateSession": {"name": "other", "folder": None}})
        attach(by_other, "other")
        attach(by_main, "main")
        t0 = tree(req)
        moved_tab, _, moved_panes = active(t0["main"])
        back_tab = t0["main"][0][0]
        check(len(moved_panes) == 2 and len(t0["main"]) == 2, f"setup: {t0}")
        pane_b = moved_panes[1]
        sub.send({"SubscribePane": {"pane_id": pane_b, "cols": 40, "rows": 10}})
        sub.drain(0.6)

        print("1. refusals change nothing")
        type_line(req, "for i in $(seq 1 120); do printf 'LINE:%s\\n' $i; done")
        req.drain(0.8)
        req.send({"ScrollDelta": {"delta": 10}})
        req.drain(0.6)
        by_main.drain(0.3)
        by_other.drain(0.3)
        refused = (
            ("an unknown tab", 99999, "main", "other"),
            ("an unknown target", moved_tab, "main", "ghost"),
            ("a stale source", moved_tab, "other", "main"),
            ("the source as target", moved_tab, "main", "main"),
        )
        for label, tab, source, target in refused:
            msgs = move(req, tab, source, target)
            check(tree(req) == t0, f"{label}: the tree is unchanged")
            check(not [m for m in msgs if name_of(m) in RENDERS and
                       only(m, name_of(m)).get("scroll_offset", 0) == 0],
                  f"{label}: the scrolled requester stayed scrolled")
        check("refused" in srv.log(), "the log says why")
        req.send({"ScrollDelta": {"delta": -10}})
        req.drain(0.6)

        print("2. the active tab moves and the requester follows")
        by_main.drain(0.3)
        by_other.drain(0.3)
        move(req, moved_tab, "main", "other")
        t1 = tree(req)
        check([t[0] for t in t1["main"]] == [back_tab], f"main keeps only Tab 1: {t1}")
        check(any(t[0] == moved_tab and t[2] == moved_panes for t in t1["other"]),
              f"other holds the tab with its id and panes: {t1}")
        check(current(req) == "other", "the requester followed the tab")
        check(active(t1["other"])[0] == moved_tab, "the moved tab is active in other")
        check(active(t1["main"])[0] == back_tab, "main falls back to its other tab")
        got = by_main.drain(0.8)
        st = status_of(got)
        check(st is not None and st["session_name"] == "main",
              f"by_main is PUSHED a frame of main ({st})")
        got = by_other.drain(0.8)
        check(rendered(got) != [], "by_other is PUSHED a frame")

        print("3. the moved panes keep working")
        by_main.drain(0.3)
        by_other.drain(0.3)
        sub.drain(0.3)
        type_line(req, "clear")
        by_other.drain(0.3)
        type_line(req, "printf 'RAN:%s\\n' b")
        time.sleep(0.4)
        got = by_other.drain(0.8)
        check("RAN:b" in text_of(got), "output reaches a client of the new session")
        check(not rendered(by_main.drain(0.5)), "main is not re-rendered for it")
        check("RAN:b" in pane_contents(sub.drain(0.8), pane_b),
              "the pane subscription, a View's stream, still carries it")
        sub.send({"InputToPane": {"pane_id": pane_b,
                                  "data": list(b"printf 'VIA:%s\\n' view\n")}})
        time.sleep(0.5)
        check("VIA:view" in pane_contents(sub.drain(0.8), pane_b),
              "InputToPane still reaches the moved pane")

        print("4. a background tab goes without taking the requester")
        attach(req, "other")
        cmd(req, "TabNew")
        t2 = tree(req)
        shown = active(t2["other"])[0]
        background = t2["other"][0][0]
        check(background != shown, f"setup: {t2}")
        by_main.drain(0.3)
        move(req, background, "other", "main")
        t3 = tree(req)
        check(any(t[0] == background for t in t3["main"]), f"the tab moved: {t3}")
        check(current(req) == "other", "the requester stayed")
        check(active(t3["other"])[0] == shown, "the source keeps its active tab")
        check(active(t3["main"])[0] == back_tab, "the target keeps its active tab")
        check(background in saved_tabs("main") and background not in saved_tabs("other"),
              "state.json has the tab in its new session")
        got = by_main.drain(0.8)
        check(rendered(got) != [], "a bystander of the target is PUSHED the new tab bar")

        print("5. the only tab of a session")
        req.send({"CreateSession": {"name": "solo", "folder": None}})
        attach(req, "solo")
        by_solo = Client(srv.sock)
        by_solo.hello()
        attach(by_solo, "solo")
        t4 = tree(req)
        solo_tab = t4["solo"][0][0]
        by_solo.drain(0.3)
        move(req, solo_tab, "solo", "other")
        t5 = tree(req)
        check("solo" not in t5, f"the emptied session is gone: {t5}")
        check(any(t[0] == solo_tab for t in t5["other"]), "its tab is in other")
        check(current(req) == "other", "the requester landed in the target")
        check(active(t5["other"])[0] == solo_tab, "on the tab it moved")
        check("solo" not in saved_sessions(), f"state.json dropped it: {saved_sessions()}")
        st = status_of(by_solo.drain(1.0))
        check(st is not None and st["session_name"] != "solo",
              f"its bystander was re-homed ({st})")
        check(current(by_solo) is not None, "its bystander is on a live session")
        by_solo.close()

        print("6. a scroll left on a moved pane does not freeze a bystander")
        attach(req, "main")
        attach(by_main, "main")
        t6 = tree(req)
        shown = active(t6["main"])[0]
        type_line(req, "for i in $(seq 1 120); do printf 'SCR:%s\\n' $i; done")
        by_main.drain(0.8)
        by_main.send({"ScrollDelta": {"delta": 10}})
        offset = [only(m, name_of(m)).get("scroll_offset", 0)
                  for m in by_main.drain(0.6) if name_of(m) in RENDERS]
        check(offset and offset[-1] == 10, f"by_main is scrolled back ({offset})")
        move(req, shown, "main", "other")
        check(current(req) == "other", "the requester followed the tab out")
        move(req, shown, "other", "main")
        check(current(req) == "main" and active(tree(req)["main"])[0] == shown,
              "and back, so the pane is by_main's focused pane again")
        by_main.drain(0.6)
        # Not `clear`: its ED 3 empties the scrollback, which clamps the offset
        # and frees the client whatever the move left behind.
        type_line(req, "printf 'LIVE:%s\\n' x")
        time.sleep(0.3)
        check(rendered(by_main.drain(1.0)) != [], "it is still pushed frames")

        print("7. an unattached requester")
        t7 = tree(loose)
        tab = t7["main"][0][0]
        move(loose, tab, "main", "other")
        check(any(t[0] == tab for t in tree(loose)["other"]), "the tab moved")
        check(current(loose) is None, "the requester stays unattached")

        print("8. no panic")
        check("panicked" not in srv.log(), "no panic in server.log")
    finally:
        for c in (req, by_main, by_other, sub, loose):
            c.close()
        srv.kill()
    if fails:
        print(f"FAILED: {len(fails)}")
        for f in fails:
            print(" -", f)
        sys.exit(1)
    print("OK")


if __name__ == "__main__":
    main()
