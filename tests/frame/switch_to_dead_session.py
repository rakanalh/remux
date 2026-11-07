"""A switch to a tab or pane of a session that no longer exists is refused.

`SessionSwitchTab` and `SessionSwitchPane` name their session, and a client
sends them from a tree that can be older than a deletion. The server used to
point the client at that name before checking it, so the client was attached to
nothing: every key it typed was dropped and no frame reached it. Each case
deletes a session, sends a switch into it, and requires:

  - an `Error` naming the session, as `Attach` gets for a dead name,
  - the client still current on its live session in the server's tree,
  - a line typed afterwards running there, read back as an assembled marker.

Run from the repo root: PYTHONPATH=tests/frame python3 tests/frame/switch_to_dead_session.py
"""
import os
import sys
import time

from harness import Server, Client, name_of, only

RUNDIR = os.environ.get("REMUX_SDS_RUN", "/tmp/rmxsds")
COLS, ROWS = 100, 30

fails = []


def check(cond, msg):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        fails.append(msg)


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


def current(cli):
    return next((s["name"] for s in tree(cli) if s.get("is_current")), None)


def errors(msgs):
    return [only(m, "Error")["message"] for m in msgs if name_of(m) == "Error"]


def text_of(msgs):
    out = []
    for m in msgs:
        n = name_of(m)
        if n == "FullRender":
            out += ["".join(c.get("c", " ") for c in row) for row in only(m, n)["cells"]]
        elif n == "RenderDiff":
            out.append("".join(ch["cell"].get("c", " ") for ch in only(m, n)["changes"]))
    return "\n".join(out)


def refused(label, cli, command, tag):
    cli.send({"CreateSession": {"name": "gone", "folder": None}})
    cli.drain(0.4)
    tab = next(s for s in tree(cli) if s["name"] == "gone")["tabs"][0]
    pane = tab["panes"][0]["id"]
    cli.send({"KillSession": {"name": "gone"}})
    cli.drain(0.5)
    args = {"session": "gone", "tab_index": 0}
    if command == "SessionSwitchPane":
        args["pane_id"] = pane
    cli.send({"Command": {command: args}})
    got = cli.drain(1.0)
    check("session 'gone' not found" in errors(got),
          f"{label}: the switch is refused by name: {errors(got)}")
    check(current(cli) == "main", f"{label}: the client is still current on main")
    cli.send({"Input": {"data": list(f"printf 'RAN:%s\\n' {tag}\n".encode())}})
    time.sleep(0.5)
    check(f"RAN:{tag}" in text_of(cli.drain(1.0)), f"{label}: typed input still runs in main")


def main():
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(1.0)
        print("1. SessionSwitchTab into a deleted session")
        refused("tab", cli, "SessionSwitchTab", "tab")
        print("2. SessionSwitchPane into a deleted session")
        refused("pane", cli, "SessionSwitchPane", "pane")
        print("3. a switch into a live session still works")
        cli.send({"CreateSession": {"name": "live", "folder": None}})
        cli.drain(0.4)
        cli.send({"Command": {"SessionSwitchTab": {"session": "live", "tab_index": 0}}})
        got = cli.drain(1.0)
        check(not errors(got), f"no error for a live session: {errors(got)}")
        check(current(cli) == "live", "the client is on the live session")
    finally:
        cli.close()
        srv.kill()
    log = open(f"{RUNDIR}/state/remux/server.log").read()
    check("panicked at" not in log, "no panic in server.log")
    print(f"\n{'FAIL' if fails else 'PASS'}: {len(fails)} failure(s)")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
