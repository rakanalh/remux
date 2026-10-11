#!/usr/bin/env python3
"""A session's last pane exiting removes the session from its folder too.

`close_pane` removes a session whose last pane closed. The folder holds the
session's name in its own list, and a removal that forgets it leaves the name
there. The session tree hides a name no live session answers to, so the stale
entry is read from state.json, and its visible symptom is checked too: a new,
unfiled session that reuses the name shows up inside the folder as well.

Two sessions share folder `work`, so the folder survives and its list can be
read: `gone` loses its only pane when its shell exits, and `kept` must be the
only name left. The pane's exit comes from the shell, so the close runs the
forwarder's death path rather than a command.

Also covered, on the same path: `gone`'s hidden popup pane is reaped with it
and its subscriber is told.

Run: python3 tests/frame/close_last_pane_folder.py
"""
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of, only  # noqa: E402

RUNDIR = "/tmp/rmxclf"

fails = []


def check(cond, msg):
    print(("PASS  " if cond else "FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def tree(cli):
    cli.send("ListSessionTree")
    for _ in range(200):
        m = cli.recv()
        if name_of(m) == "SessionTree":
            return m["SessionTree"]
    return None


def folder_sessions(t, folder):
    for f in t.get("folders", []):
        if f["name"] == folder:
            return [s["name"] for s in f.get("sessions", [])]
    return None


def session_names(t):
    names = [s["name"] for s in t.get("unfiled", [])]
    for f in t.get("folders", []):
        names += [s["name"] for s in f.get("sessions", [])]
    return names


def saved_folder(folder):
    """The folder's session list in state.json, which is the raw list: the
    session tree is built from live sessions and never shows a stale name."""
    try:
        with open(f"{RUNDIR}/data/remux/state.json") as f:
            return json.load(f)["state"]["folders"][folder]["session_ids"]
    except (OSError, KeyError, ValueError):
        return None


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli, sub = Client(srv.sock), Client(srv.sock)
    try:
        cli.hello()
        sub.hello()
        cli.send({"CreateSession": {"name": "kept", "folder": "work"}})
        cli.send({"CreateSession": {"name": "gone", "folder": "work"}})
        cli.send({"Attach": {"session_name": "gone"}})
        cli.send({"Resize": {"cols": 100, "rows": 30}})
        cli.drain(0.8)
        cli.send({"Command": "PopupToggle"})
        cli.drain(0.6)
        cli.send({"Command": "PopupToggle"})
        cli.drain(0.6)
        popups = re.findall(r'PopupToggle session="gone" spawned popup pane_id=(\d+)', srv.log())
        check(len(popups) == 1, f"setup: one popup was spawned for gone ({popups})")
        popup = int(popups[0]) if popups else None
        if popup is not None:
            sub.send({"SubscribePane": {"pane_id": popup, "cols": 40, "rows": 12,
                                        "size_demand": False}})
            sub.drain(0.6)
        t0 = tree(cli)
        check(sorted(folder_sessions(t0, "work") or []) == ["gone", "kept"],
              f"setup: folder work lists both sessions ({folder_sessions(t0, 'work')})")

        cli.send({"Input": {"data": list(b"exit\n")}})
        t1 = None
        for _ in range(20):
            time.sleep(0.2)
            t1 = tree(cli)
            if "gone" not in session_names(t1):
                break
        print(f"  folders after the exit: {t1.get('folders')}")
        check("gone" not in session_names(t1), "the session whose last pane exited is gone")
        check(folder_sessions(t1, "work") == ["kept"],
              f"folder work no longer lists it ({folder_sessions(t1, 'work')})")
        saved = saved_folder("work")
        check(saved == ["kept"], f"the saved folder no longer lists it ({saved})")
        cli.send({"CreateSession": {"name": "gone", "folder": None}})
        cli.drain(0.6)
        t2 = tree(cli)
        unfiled = [s["name"] for s in t2.get("unfiled", [])]
        check("gone" in unfiled and folder_sessions(t2, "work") == ["kept"],
              f"a new unfiled session of the same name is not claimed by the folder "
              f"(unfiled={unfiled}, work={folder_sessions(t2, 'work')})")
        exited = [only(only(m, "Event"), "PaneExited") for m in sub.drain(1.0)
                  if name_of(m) == "Event" and name_of(only(m, "Event")) == "PaneExited"]
        check(popup is not None and any(e["pane_id"] == popup for e in exited),
              f"its popup pane was reaped and its subscriber told ({exited})")
    except (OSError, ConnectionError) as e:
        check(False, f"the server stopped answering: {e!r}")
    finally:
        cli.close()
        sub.close()
        srv.kill()
    check("panicked at" not in srv.log(), "no panic in server.log")

    print()
    if fails:
        print(f"FAIL: {len(fails)} check(s) failed")
        raise SystemExit(1)
    print("OK")


if __name__ == "__main__":
    main()
