#!/usr/bin/env python3
"""An OPEN session manager must learn about a session created elsewhere.

Finding 9: `SessionEvent::SessionCreated` was sent (to the creating client only)
and the client's sole `Event` handler gated on `SessionDeleted`, so it fell
through silently. Every one of the ~20 `ListSessionTree` refreshes is
event-driven -- there is no timer -- so an open session manager never learned
about a session another terminal created until some unrelated action happened to
refresh the tree.

Two halves were needed and both are exercised here:
  * the server now announces `SessionCreated` to EVERY client, not just the
    creator (a creator-only event can never reach the stale terminal);
  * the client now handles it symmetrically with `SessionDeleted` and refreshes
    the tree -- without arming `pending_manager_exit_check`, which only a
    deletion may do.

Client A is the real TUI with its session manager open. Client B is a second
connection to the SAME server, speaking the wire protocol directly. B creates a
session; A is then left completely alone (no keystrokes at all) and must show it.

Run from the repo root:  python3 tests/pty/session_created_refreshes_manager.py
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from pty_harness import Tui  # noqa: E402
from harness import Client  # noqa: E402

NEW_SESSION = "zulu_from_b"


def main():
    t = Tui("/tmp/rmxscr", cols=120, rows=40).start()
    fails = []
    b = None
    try:
        # --- A: open the session manager and hold it open ---------------------
        t.prefix(b"xm", 1.2)
        # The manager opens on the tree (`/` is the way into the search bar), and
        # this test sends no keystrokes at all past this point on purpose.
        t.pump(0.9)
        if not t.has("Session Manager"):
            print("ABORT: the session manager never opened")
            t.dump("no session manager")
            t.kill()
            sys.exit(1)
        if t.has(NEW_SESSION):
            print(f"ABORT: {NEW_SESSION!r} exists before B created it")
            t.dump("pre-existing session")
            t.kill()
            sys.exit(1)
        print("A: session manager open, tree does not list the new session yet")

        # --- B: a second client on the same server creates a session ----------
        b = Client(f"{t.rundir}/run/remux.sock")
        b.hello()
        b.drain(0.3)
        b.send({"CreateSession": {"name": NEW_SESSION, "folder": None}})

        # --- A does NOTHING. No keystroke, no resize, no command. -------------
        t.pump(2.5)

        if not t.has(NEW_SESSION):
            fails.append(f"A's open session manager never showed {NEW_SESSION!r} "
                         "-- the tree is still stale")
        if not t.has("Session Manager"):
            fails.append("A's session manager closed itself on SessionCreated")
        if not t.alive():
            fails.append("A exited on SessionCreated (the exit check was armed "
                         "by a creation, which must never happen)")

        logs = (t.log("client") + t.log("server")).lower()
        if "panic" in logs:
            fails.append("panic in the logs")

        if fails:
            print("\nFAILURES:")
            for f in fails:
                print(f"  - {f}")
            t.dump("final")
            print("RESULT: FAIL")
            sys.exit(1)
        t.dump("final")
        print(f"\nRESULT: PASS -- B created {NEW_SESSION!r} and A's open session "
              "manager refreshed itself with no input from A")
    finally:
        if b is not None:
            b.close()
        t.kill()


if __name__ == "__main__":
    main()
