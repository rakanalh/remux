"""Removing a session re-homes the clients on it, and only an empty server ends them.

`KillSession` and `FolderDelete` used to send `SessionDeleted` to every client on
the removed session, and a foreground client reads that as "nothing is left" and
quits. Each case here has survivors, so that event must not appear until the
last case, where it must.

  1. The requester kills its own session: it is attached to a survivor, and is
     told that session's border style, which only `handle_attach` sends.
  2. A bystander on a session killed by an unattached requester is PUSHED a
     frame of a survivor. The requester stays unattached.
  3. A requester and a bystander on one of a folder's two sessions, a third
     client on the other: all land on the one survivor, whichever order the
     removals run in.
  4. A client on an unrelated session is left where it is.
  5. Nothing left: a client on the last session gets `SessionDeleted` and is
     attached to nothing. The same through a folder.
  6. The survivor was never attached, so its pane has no forwarding task until
     a bystander lands there. The bystander is told the border style before the
     first frame, and a marker its shell prints reaches it. The survivor is the
     only session left each time, because `sessions` is a HashMap and any other
     choice would be arbitrary. The kill path and the folder cascade both.

Run from the repo root: PYTHONPATH=tests/frame python3 tests/frame/kill_attached_session.py
"""
import os
import sys
import time

from harness import Server, Client, name_of, only
from rundirs import run_dir  # noqa: E402

RUNDIR = os.environ.get("REMUX_KAS_RUN") or run_dir("rmxkas")
COLS, ROWS = 100, 30
RENDERS = ("FullRender", "RenderDiff", "ScrollRender")

fails = []


def check(cond, msg):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        fails.append(msg)


def attach(cli, session):
    cli.send({"Attach": {"session_name": session}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.7)


def create(cli, name, folder=None):
    cli.send({"CreateSession": {"name": name, "folder": folder}})
    cli.drain(0.4)


def tree(cli):
    cli.send("ListSessionTree")
    for _ in range(400):
        m = cli.recv()
        if name_of(m) == "SessionTree":
            t = m["SessionTree"]
            entries = list(t.get("unfiled", []))
            for f in t.get("folders", []):
                entries.extend(f.get("sessions", []))
            return entries
    raise SystemExit("no SessionTree reply")


def names(cli):
    return sorted(s["name"] for s in tree(cli))


def current(cli):
    return next((s["name"] for s in tree(cli) if s.get("is_current")), None)


def deleted_events(msgs):
    return [m for m in msgs if name_of(m) == "Event"
            and "SessionDeleted" in str(only(m, "Event"))]


def frame_session(msgs):
    status = None
    for m in msgs:
        n = name_of(m)
        if n in RENDERS:
            status = only(m, n).get("status") or status
    return status["session_name"] if status else None


def text_of(msgs):
    out = []
    for m in msgs:
        n = name_of(m)
        if n == "FullRender":
            out += ["".join(c.get("c", " ") for c in row) for row in only(m, n)["cells"]]
        elif n == "RenderDiff":
            out.append("".join(ch["cell"].get("c", " ") for ch in only(m, n)["changes"]))
    return "\n".join(out)


def check_landing(label, cli, survivor, got, tag):
    kinds = [name_of(m) for m in got]
    frames = [i for i, k in enumerate(kinds) if k in RENDERS]
    check("SessionBorderStyle" in kinds and frames
          and kinds.index("SessionBorderStyle") < frames[0],
          f"{label}: the border style comes before the first frame: {kinds}")
    check(current(cli) == survivor, f"{label}: the bystander is on {survivor}")
    cli.send({"Input": {"data": list(f"printf 'RAN:%s\\n' {tag}\n".encode())}})
    time.sleep(0.5)
    check(f"RAN:{tag}" in text_of(cli.drain(1.0)),
          f"{label}: the survivor's pane reaches the bystander")


def main():
    srv = Server(RUNDIR).start()
    req, by, third, loose = (Client(srv.sock) for _ in range(4))
    try:
        for c in (req, by, third, loose):
            c.hello()

        print("1. the requester kills its own session")
        create(req, "keep")
        create(req, "one")
        attach(req, "one")
        req.send({"KillSession": {"name": "one"}})
        got = req.drain(1.0)
        check(not deleted_events(got), "no SessionDeleted while a session survives")
        check(current(req) == "keep", "it is attached to the survivor")
        check(any(name_of(m) == "SessionBorderStyle" for m in got),
              "it was told the survivor's border style")
        check(frame_session(got) == "keep", "it is sent a frame of the survivor")

        print("2. a bystander on a session killed from outside")
        create(loose, "two")
        attach(by, "two")
        loose.send({"KillSession": {"name": "two"}})
        got = by.drain(1.0)
        check(not deleted_events(got), "the bystander gets no SessionDeleted")
        check(frame_session(got) == "keep", "the bystander is PUSHED a frame of the survivor")
        check(current(by) == "keep", "the bystander is attached to the survivor")
        check(current(loose) is None, "the requester stays unattached")

        print("3. a folder holding several clients' sessions")
        loose.send({"Command": {"FolderNew": "f"}})
        loose.drain(0.3)
        create(loose, "fa", "f")
        create(loose, "fb", "f")
        attach(req, "fa")
        attach(by, "fa")
        attach(third, "fb")
        req.send({"Command": {"FolderDelete": "f"}})
        msgs = {"req": req.drain(1.0), "by": by.drain(1.0), "third": third.drain(1.0)}
        check(names(loose) == ["keep"], f"only the survivor is left: {names(loose)}")
        for label, cli in (("req", req), ("by", by), ("third", third)):
            check(not deleted_events(msgs[label]), f"{label} gets no SessionDeleted")
            check(frame_session(msgs[label]) == "keep", f"{label} is sent a frame of keep")
            check(current(cli) == "keep", f"{label} is attached to keep")

        print("4. a client elsewhere stays")
        create(loose, "other")
        attach(third, "other")
        third.drain(0.3)
        create(loose, "keep2")
        attach(by, "keep2")
        req.send({"KillSession": {"name": "keep2"}})
        req.drain(0.6)
        check(current(third) == "other", "the unrelated client did not move")
        attach(req, "keep")
        attach(by, "keep")
        attach(third, "keep")
        loose.send({"KillSession": {"name": "other"}})
        loose.drain(0.5)

        print("5. nothing left")
        req.send({"KillSession": {"name": "keep"}})
        got_req, got_by = req.drain(1.0), by.drain(1.0)
        check(len(deleted_events(got_req)) == 1, "the requester gets SessionDeleted")
        check(len(deleted_events(got_by)) == 1, "the bystander gets SessionDeleted")
        check(current(req) is None and current(by) is None, "both are attached to nothing")
        check(names(loose) == [], "the server holds no session")

        loose.send({"Command": {"FolderNew": "g"}})
        loose.drain(0.3)
        create(loose, "ga", "g")
        create(loose, "gb", "g")
        attach(req, "ga")
        attach(by, "gb")
        req.send({"Command": {"FolderDelete": "g"}})
        got_req, got_by = req.drain(1.0), by.drain(1.0)
        check(len(deleted_events(got_req)) == 1, "a folder emptying the server tells the requester")
        check(len(deleted_events(got_by)) == 1, "and the client on its other session")

        print("6. the survivor was never attached")
        create(loose, "cold")
        create(loose, "doomed")
        attach(by, "doomed")
        loose.send({"KillSession": {"name": "doomed"}})
        check_landing("kill", by, "cold", by.drain(1.0), "kill")

        create(loose, "cold2")
        loose.send({"Command": {"FolderNew": "h"}})
        loose.drain(0.3)
        create(loose, "fdoomed", "h")
        attach(by, "fdoomed")
        loose.send({"KillSession": {"name": "cold"}})
        loose.drain(0.5)
        loose.send({"Command": {"FolderDelete": "h"}})
        check_landing("folder", by, "cold2", by.drain(1.0), "folder")

        check("panicked" not in srv.log(), "no panic in server.log")
    finally:
        for c in (req, by, third, loose):
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
