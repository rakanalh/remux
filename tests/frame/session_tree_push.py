#!/usr/bin/env python3
"""Frame-level test: server-pushed session tree (PROTOCOL_VERSION 5).

Covers `SubscribeSessionTree` / `UnsubscribeSessionTree`:

  1. subscribing yields an immediate `SessionTree` (the panel is populated at
     once, not on the next change);
  2. a structural change made by a DIFFERENT connection reaches the subscriber
     unsolicited;
  3. a burst of structural commands coalesces to fewer pushes than commands;
  4. `UnsubscribeSessionTree` stops them;
  5. an old-protocol handshake still gets a Welcome stamped with the CURRENT
     protocol version -- skew stays detectable -- and leaves the server alive.

Run: python3 tests/frame/session_tree_push.py
"""
import json
import os
import socket
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import PROTOCOL_VERSION, Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-stp"
FAILURES = []


def check(cond, msg):
    if cond:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        FAILURES.append(msg)


def trees(msgs):
    return [m for m in msgs if name_of(m) == "SessionTree"]


def session_names(tree_msg):
    body = tree_msg["SessionTree"]
    names = [e["name"] for e in body["unfiled"]]
    for f in body["folders"]:
        names += [e["name"] for e in f["sessions"]]
    return names


def wait_for_tree(c, timeout=1.0):
    """Collect messages until a SessionTree arrives or `timeout` elapses."""
    end = time.time() + timeout
    while time.time() < end:
        for m in c.drain(0.1):
            if name_of(m) == "SessionTree":
                return m
    return None


def main():
    srv = Server(RUNDIR).start()
    try:
        try:
            run(srv)
        except (BrokenPipeError, ConnectionError) as e:
            # Before the feature exists the server drops the connection on the
            # undecodable message; report that as a failure rather than a
            # traceback so the remaining checks still get named.
            check(False, f"the connection stayed alive throughout ({e})")
    finally:
        srv.kill()

    print()
    if FAILURES:
        print(f"FAILED ({len(FAILURES)}): " + "; ".join(FAILURES))
        return 1
    print("session_tree_push: all checks passed")
    return 0


def run(srv):
    if True:
        # c1 only subscribes; it never attaches, so every SessionTree it sees is
        # an unsolicited push rather than a side effect of being rendered to.
        c1 = Client(srv.sock)
        c2 = Client(srv.sock)
        c1.hello()
        c2.hello()

        # c2 is the actor: a real session with a live pane to mutate.
        c2.send({"CreateSession": {"name": "actor", "folder": None}})
        c2.send({"Attach": {"session_name": "actor"}})
        c2.send({"Resize": {"cols": 100, "rows": 30}})
        c2.drain(0.5)
        c1.drain(0.3)

        # --- 1. immediate tree on subscribe -----------------------------------
        c1.send("SubscribeSessionTree")
        first = wait_for_tree(c1, 1.0)
        check(first is not None, "subscribe answers with an immediate SessionTree")
        if first is not None:
            check(
                "actor" in session_names(first),
                f"the immediate tree carries live sessions: {session_names(first)}",
            )

        # --- 2. an unsolicited push after another connection changes things ----
        c2.send({"CreateSession": {"name": "pushed", "folder": None}})
        msg = wait_for_tree(c1, 1.0)
        check(msg is not None, "unsolicited SessionTree after a structural change")
        if msg is not None:
            names = session_names(msg)
            check("pushed" in names, f"the push carries the new session: {names}")

        # --- 3. a burst of structural commands coalesces -----------------------
        c1.drain(0.4)  # settle: swallow any trailing push still in flight
        for _ in range(5):
            c2.send({"Command": "TabNew"})
        burst = trees(c1.drain(0.9))
        check(
            1 <= len(burst) < 5,
            f"5 structural commands coalesce to fewer pushes: got {len(burst)}",
        )
        # Coalescing must not swallow the END of a burst: a leading-edge-only
        # gate would push the state after the FIRST command and drop the rest,
        # which is the state that actually matters. The last push must show all
        # five new tabs (the session started with one).
        if burst:
            actor = [
                e for e in burst[-1]["SessionTree"]["unfiled"] if e["name"] == "actor"
            ]
            tabs = len(actor[0]["tabs"]) if actor else -1
            check(tabs == 6, f"the trailing push carries the burst's final state: {tabs} tabs, want 6")

        # --- 3b. mere rendering is not a structural change ---------------------
        # `update_auto_pane_names` runs on every render and every mouse event.
        # It must only mark the tree dirty when a pane's process name ACTUALLY
        # changed; notifying unconditionally would be a push storm that the
        # coalescing merely hides (one push per interval, forever, while the
        # user types). Bare newlines make the shell repaint without ever
        # changing what the tree displays.
        c1.drain(0.4)
        for _ in range(6):
            c2.send({"Input": {"data": list(b"\n")}})
            time.sleep(0.08)
        c2.drain(0.1)
        idle = trees(c1.drain(0.6))
        check(len(idle) == 0, f"repainting alone pushes nothing: got {len(idle)}")

        # --- 3bb. click-to-focus is a tree change ------------------------------
        # `PaneTreeEntry.is_focused` is in the payload, so a mouse focus change
        # must push exactly as a keyboard one does. Under polling this staleness
        # was invisible; under push it is precisely what the feature promises.
        def split_tab(msg):
            """The `actor` tab holding two panes, as {pane_id: is_focused}."""
            for e in msg["SessionTree"]["unfiled"]:
                if e["name"] != "actor":
                    continue
                for t in e["tabs"]:
                    if len(t["panes"]) == 2:
                        return {p["id"]: p["is_focused"] for p in t["panes"]}
            return None

        c2.send({"Command": "PaneSplitVertical"})
        before = wait_for_tree(c1, 1.5)
        panes_before = split_tab(before) if before else None
        check(panes_before is not None, "a vertical split shows two panes in the pushed tree")
        if panes_before:
            focused = [pid for pid, f in panes_before.items() if f]
            unfocused = [pid for pid, f in panes_before.items() if not f]
            # Click deep inside the left half; whichever pane lives there, the
            # assertion below only cares that focus MOVED and was pushed.
            c1.drain(0.4)
            c2.send({"MouseClick": {"x": 5, "y": 5, "pane_id": None, "release": False}})
            after = wait_for_tree(c1, 1.5)
            panes_after = split_tab(after) if after else None
            moved = (
                panes_after is not None
                and panes_before is not None
                and panes_after != panes_before
            )
            check(
                moved,
                f"click-to-focus pushes the new is_focused: {panes_before} -> {panes_after}",
            )
            # Guard the guard: if the click had landed on the already-focused
            # pane there would be nothing to detect, and the check above would
            # be vacuous rather than passing.
            check(
                bool(focused) and bool(unfocused),
                "the split really did leave one pane focused and one not",
            )

        # --- 3c. a REAL process-name change does push --------------------------
        # The mirror of 3b, and the half that would rot silently: if
        # `get_pane_name`'s semantics ever drift, the gate goes permanently
        # false and pane-label pushes stop, with every other check still green.
        # `get_process_name` reads the pane's own /proc/<pid>/comm, so a plain
        # `sleep` (a child) would not change it -- `exec` replaces the shell's
        # process image in place, which does.
        c1.drain(0.4)
        c2.send({"Input": {"data": list(b"exec sleep 60\n")}})
        time.sleep(0.5)
        # Force a render WITHOUT a structural command: `Resize` re-runs
        # `update_auto_pane_names` but never marks the tree dirty itself, so the
        # push below can only come from the name gate.
        c2.send({"Resize": {"cols": 96, "rows": 30}})
        named = wait_for_tree(c1, 1.5)
        pane_names = []
        if named is not None:
            for e in named["SessionTree"]["unfiled"]:
                if e["name"] == "actor":
                    for t in e["tabs"]:
                        pane_names += [p["name"] for p in t["panes"]]
        check(
            named is not None and "sleep" in pane_names,
            f"a real process-name change pushes, naming the new process: {pane_names}",
        )

        # --- 3d/3e. attach and detach change the tree --------------------------
        # `client_count` and `is_current` are rendered from the client map, so
        # attaching/detaching changes what EVERY subscriber sees.
        def actor_count(msgs):
            for m in reversed(msgs):
                for e in m["SessionTree"]["unfiled"]:
                    if e["name"] == "actor":
                        return e["client_count"]
            return None

        c1.drain(0.4)
        c2.send("Detach")
        detached = actor_count(trees(c1.drain(0.8)))
        check(detached == 0, f"Detach pushes an updated client_count: got {detached}, want 0")

        c1.drain(0.4)
        c2.send({"Attach": {"session_name": "actor"}})
        c2.send({"Resize": {"cols": 100, "rows": 30}})
        attached = actor_count(trees(c1.drain(0.8)))
        check(attached == 1, f"Attach pushes an updated client_count: got {attached}, want 1")

        # --- 4. unsubscribe stops the pushes ----------------------------------
        c1.send("UnsubscribeSessionTree")
        c1.drain(0.4)  # let any push scheduled before the unsubscribe land
        c2.send({"CreateSession": {"name": "after-unsub", "folder": None}})
        c2.send({"Command": "TabNew"})
        after = trees(c1.drain(0.8))
        check(len(after) == 0, f"no pushes after UnsubscribeSessionTree: got {len(after)}")

        # A client that never subscribed must see no pushes at all, and the
        # request/response path must still work exactly as before.
        c3 = Client(srv.sock)
        c3.hello()
        c3.drain(0.3)
        c2.send({"Command": "TabNew"})
        check(
            len(trees(c3.drain(0.6))) == 0,
            "a client that never subscribed receives no pushes",
        )
        c3.send("ListSessionTree")
        replied = wait_for_tree(c3, 1.5)
        check(replied is not None, "ListSessionTree still answers on request")
        if replied is not None:
            check(
                "after-unsub" in session_names(replied),
                "the ListSessionTree reply is still the full current tree",
            )

        c1.close()
        c2.close()
        c3.close()

        # --- 5. old-protocol handshake stays cleanly detectable ---------------
        # The server is deliberately lenient (daemon.rs logs the mismatch and
        # proceeds); the HARD reject lives client-side in terminal.rs. What the
        # server must guarantee is that the skew is *detectable*: the Welcome
        # carries the server's real version, and the daemon survives.
        # The ONLY hard-coded protocol_version left in the harnesses, and it is
        # deliberate: this is the SKEW test, so the number must be a version that
        # is not the current one. The `PROTOCOL_VERSION != 4` check below is what
        # keeps it honest if the real version ever reaches 4 from the other
        # direction. Do not "sweep" this one onto the shared reader.
        old = socket.socket(socket.AF_UNIX)
        old.connect(srv.sock)
        old.settimeout(2.0)
        body = json.dumps({"protocol_version": 4, "remux_version": "old"}).encode()
        old.sendall(struct.pack(">I", len(body)) + body)
        hdr = old.recv(4)
        n = struct.unpack(">I", hdr)[0]
        payload = b""
        while len(payload) < n:
            payload += old.recv(n - len(payload))
        welcome = json.loads(payload)
        check(
            welcome.get("protocol_version") == PROTOCOL_VERSION,
            f"an old Hello still gets a Welcome stamped v{PROTOCOL_VERSION}: {welcome}",
        )
        check(
            PROTOCOL_VERSION != 4,
            "the harness speaks the bumped PROTOCOL_VERSION, not the old one",
        )
        old.close()

        # The daemon survived the skewed peer and still serves fresh clients.
        c4 = Client(srv.sock)
        c4.hello()
        c4.send("ListSessionTree")
        check(
            wait_for_tree(c4, 1.5) is not None,
            "the server still serves clients after a skewed handshake",
        )
        c4.close()

        log = srv.log()
        check("panicked" not in log, "no panic in the server log")


if __name__ == "__main__":
    sys.exit(main())
